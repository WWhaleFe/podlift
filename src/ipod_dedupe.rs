//! 아이팟 안의 중복 곡 정리. 중복 찾기(dedupe.rs)는 아이팟 음악 폴더를 그대로 훑고,
//! 여기서는 고른 결과대로 곡 DB 에서 곡을 빼고 파일을 치운다.
//!
//! - 뺄 곡이 든 재생 목록에는 남길 곡을 대신 넣고, 재생 횟수·별점은 남길 곡으로 합친다(itunesdb.rs).
//! - 파일은 지우지 않고 iPod_Control/Music/.podlift-old/Fxx/ 로 옮겨 둔다. '마지막 작업 되돌리기'로
//!   DB·재생 횟수 파일·파일을 작업 전으로 돌릴 수 있고, 다음 넣기 작업을 시작할 때 비운다.
//! - 곡 DB 에 없는 파일(아이팟 화면에 안 보이는 떠도는 파일)은 DB 를 건드리지 않고 파일만 옮긴다.
use crate::dedupe::Group;
use crate::itunesdb::DbTrack;
use crate::sync::{self, LastJob, LAST_JOB, OLD_DIR, PLAY_COUNTS};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};

/// 빼기 계획
#[derive(Clone, Debug, Default)]
pub struct Plan {
    /// (뺄 곡 번호, 대신 남길 곡 번호 - 모두 지우면 None, 뺄 곡 파일)
    pub remove: Vec<(u32, Option<u32>, PathBuf)>,
    /// 곡 DB 에 없는 버릴 파일
    pub orphans: Vec<PathBuf>,
    /// 남길 곡이 곡 DB 에 없어 건너뛴 묶음 수
    pub skipped: usize,
}

/// 파일 경로로 곡을 찾는 표 (FAT32 는 대소문자를 가리지 않으니 소문자로)
fn by_path(mount: &Path, tracks: &[DbTrack]) -> HashMap<String, u32> {
    tracks.iter().filter_map(|t| Some((t.file_path(mount)?.to_string_lossy().to_lowercase(), t.id))).collect()
}

/// 추천대로: 추천한 곡 하나만 남긴다
pub fn keep_only(g: &Group) -> Vec<bool> {
    (0..g.items.len()).map(|i| i == g.keep).collect()
}

/// 묶음마다 곡별 남기기 표시를 받아 계획을 세운다. None 인 묶음은 건드리지 않는다.
/// 여러 곡을 남기면 빼는 곡의 재생 목록 자리는 남기는 곡 중 추천한 곡(없으면 첫 곡)이 대신한다.
/// 모두 지우면 대신할 곡 없이 뺀다.
pub fn plan(mount: &Path, tracks: &[DbTrack], groups: &[(Group, Option<Vec<bool>>)]) -> Plan {
    let ids = by_path(mount, tracks);
    let id_of = |p: &Path| ids.get(&p.to_string_lossy().to_lowercase()).copied();
    let mut out = Plan::default();
    for (g, keeps) in groups {
        let Some(keeps) = keeps else { continue };
        let kept: Vec<usize> = (0..g.items.len()).filter(|&i| keeps.get(i).copied().unwrap_or(true)).collect();
        let heir = if kept.contains(&g.keep) { Some(g.keep) } else { kept.first().copied() };
        let heir_id = match heir {
            Some(h) => match id_of(&g.items[h].path) {
                Some(id) => Some(id),
                None => {
                    // 남길 곡이 화면에 안 보이는 파일이면 묶음을 건드리지 않는다
                    out.skipped += 1;
                    continue;
                }
            },
            None => None,
        };
        for (i, it) in g.items.iter().enumerate() {
            if kept.contains(&i) {
                continue;
            }
            match id_of(&it.path) {
                Some(id) if Some(id) != heir_id => out.remove.push((id, heir_id, it.path.clone())),
                Some(_) => {}
                None => out.orphans.push(it.path.clone()),
            }
        }
    }
    out
}

/// 고른 곡 파일들을 아이팟에서 지우는 계획 (대신할 곡 없이 뺀다). 곡 DB 에 없는 파일은 파일만 치운다.
pub fn plan_delete(mount: &Path, tracks: &[DbTrack], paths: &[PathBuf]) -> Plan {
    let ids = by_path(mount, tracks);
    let mut out = Plan::default();
    for p in paths {
        match ids.get(&p.to_string_lossy().to_lowercase()) {
            Some(&id) => out.remove.push((id, None, p.clone())),
            None => out.orphans.push(p.clone()),
        }
    }
    out
}

/// 백그라운드에서 고른 곡 파일들을 아이팟에서 지운다 (되돌리기 가능)
pub fn start_delete(mount: PathBuf, paths: Vec<PathBuf>) -> Job {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = (|| {
            let tracks = sync::open_db(&mount)?.tracks();
            let plan = plan_delete(&mount, &tracks, &paths);
            let fwid = crate::ipod::firewire_id(&mount).and_then(|s| crate::itunesdb::parse_firewire_id(&s));
            run(&mount, &plan, fwid.as_deref())
        })();
        let _ = tx.send(result);
    });
    Job { rx }
}

/// 정리 결과
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Outcome {
    /// 곡 DB 에서 뺀 곡 수
    pub removed: usize,
    /// 치워 둔 파일 수 (DB 에 없던 파일 포함)
    pub moved: usize,
    /// 옮기지 못한 파일 (DB 에서는 빠졌으니 아이팟에 보이지 않는다)
    pub failed: Vec<(PathBuf, String)>,
    /// 정리한 파일 (화면 목록에서 뺄 때 쓴다)
    pub done: Vec<PathBuf>,
    /// 남길 곡이 DB 에 없어 건너뛴 묶음 수
    pub skipped: usize,
}

pub struct Job {
    rx: Receiver<Result<Outcome, String>>,
}

impl Job {
    pub fn poll(&self) -> Option<Result<Outcome, String>> {
        self.rx.try_recv().ok()
    }
}

/// 백그라운드에서 곡 DB 를 읽어 계획을 세우고 정리한다. groups 는 (묶음, 곡별 남기기 - None 이면 그대로 둠).
pub fn start(mount: PathBuf, groups: Vec<(Group, Option<Vec<bool>>)>) -> Job {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = (|| {
            let tracks = sync::open_db(&mount)?.tracks();
            let plan = plan(&mount, &tracks, &groups);
            let fwid = crate::ipod::firewire_id(&mount).and_then(|s| crate::itunesdb::parse_firewire_id(&s));
            let mut out = run(&mount, &plan, fwid.as_deref())?;
            out.skipped = plan.skipped;
            Ok(out)
        })();
        let _ = tx.send(result);
    });
    Job { rx }
}

/// 계획대로 정리한다. 순서: 백업 → DB 에서 빼기 → 새 DB 쓰기 → 재생 횟수 파일 고치기 → 파일 치우기 → 작업 기록.
pub fn run(mount: &Path, plan: &Plan, fwid: Option<&[u8]>) -> Result<Outcome, String> {
    let mut db = sync::open_db(mount)?;
    if db.hashing_scheme() == 1 && fwid.is_none() {
        return Err("이 아이팟의 고유 번호(FirewireGuid)를 찾지 못해 곡 DB 에 서명할 수 없습니다. 아이팟을 다시 연결해 보세요.".into());
    }
    let itunes = sync::itunes_dir(mount);
    let music = mount.join("iPod_Control").join("Music");
    let old_dir = music.join(OLD_DIR);
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();

    // 1) 백업 (지난 작업의 되돌리기 정보는 여기서 끝난다)
    let backup = format!("iTunesDB.podlift-{stamp}");
    std::fs::copy(sync::db_path(mount), itunes.join(&backup)).map_err(|e| format!("DB 백업에 실패해 멈췄습니다: {e}"))?;
    let pc_path = itunes.join(PLAY_COUNTS);
    let playcounts_backup = if pc_path.is_file() {
        let name = format!("Play Counts.podlift-{stamp}");
        std::fs::copy(&pc_path, itunes.join(&name)).map_err(|e| format!("재생 횟수 파일 백업에 실패해 멈췄습니다: {e}"))?;
        Some(name)
    } else {
        None
    };
    let _ = std::fs::remove_dir_all(&old_dir);
    let _ = std::fs::remove_file(itunes.join(LAST_JOB));

    // 2) DB 에서 빼고 쓴다
    let mut out = Outcome::default();
    let mut removed_pos = Vec::new();
    let mut written = None;
    if !plan.remove.is_empty() {
        let pairs: Vec<(u32, Option<u32>)> = plan.remove.iter().map(|(gone, keep, _)| (*gone, *keep)).collect();
        removed_pos = db.remove_tracks(&pairs)?;
        written = Some(sync::write_db(mount, &mut db, fwid)?);
        out.removed = pairs.len();
    }
    let mut job = LastJob { backup, playcounts_backup, written, ..Default::default() };

    // 3) 재생 횟수 파일: 곡 목록 순서대로라 뺀 곡의 항목을 같이 뺀다
    if !removed_pos.is_empty() && pc_path.is_file() {
        if let Err(e) = prune_play_counts(&pc_path, &removed_pos) {
            // 순서가 어긋난 재생 횟수가 엉뚱한 곡에 붙지 않도록 지운다 (백업은 남아 있다)
            let _ = std::fs::remove_file(&pc_path);
            eprintln!("재생 횟수 파일 정리 실패: {e}");
        }
    }

    // 4) 파일 치우기: .podlift-old/Fxx/이름
    for path in plan.remove.iter().map(|(_, _, p)| p).chain(plan.orphans.iter()) {
        let sub = path.parent().and_then(|d| d.file_name()).map(PathBuf::from).unwrap_or_default();
        let Some(name) = path.file_name() else { continue };
        let parked = old_dir.join(&sub).join(name);
        let moved = std::fs::create_dir_all(old_dir.join(&sub)).and_then(|_| std::fs::rename(path, &parked));
        match moved {
            Ok(()) => {
                job.moved_old.push((sync::rel(mount, path), sync::rel(mount, &parked)));
                out.moved += 1;
                out.done.push(path.clone());
            }
            Err(e) => out.failed.push((path.clone(), e.to_string())),
        }
    }
    let _ = std::fs::write(itunes.join(LAST_JOB), serde_json::to_string_pretty(&job).unwrap_or_default());
    Ok(out)
}

/// 재생 횟수 파일(mhdp 머리 + 곡마다 같은 길이의 항목)에서 positions 번째 항목을 뺀다.
fn prune_play_counts(path: &Path, positions: &[usize]) -> Result<(), String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    if data.get(..4) != Some(b"mhdp") || data.len() < 16 {
        return Err("머리가 mhdp 가 아닙니다".into());
    }
    let rd = |at: usize| u32::from_le_bytes(data[at..at + 4].try_into().unwrap()) as usize;
    let (hlen, entry, count) = (rd(4), rd(8), rd(12));
    if entry == 0 || hlen + entry * count > data.len() {
        return Err("길이가 맞지 않습니다".into());
    }
    let gone: std::collections::HashSet<usize> = positions.iter().copied().collect();
    let mut out = data[..hlen].to_vec();
    let mut kept = 0u32;
    for i in 0..count {
        if !gone.contains(&i) {
            out.extend_from_slice(&data[hlen + i * entry..hlen + (i + 1) * entry]);
            kept += 1;
        }
    }
    out.extend_from_slice(&data[hlen + entry * count..]);
    out[12..16].copy_from_slice(&kept.to_le_bytes());
    let tmp = path.with_extension("podlift-writing");
    std::fs::write(&tmp, &out).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::AudioInfo;
    use crate::itunesdb::{tests::empty_db, ITunesDb, NewTrack};

    #[test]
    fn play_counts_prune() {
        let dir = std::env::temp_dir().join(format!("podlift-pc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Play Counts");
        let mut data = b"mhdp".to_vec();
        for v in [0x60u32, 4, 3] {
            data.extend_from_slice(&v.to_le_bytes());
        }
        data.resize(0x60, 0);
        for v in [10u32, 11, 12] {
            data.extend_from_slice(&v.to_le_bytes());
        }
        std::fs::write(&path, &data).unwrap();
        prune_play_counts(&path, &[1]).unwrap();
        let d = std::fs::read(&path).unwrap();
        assert_eq!(u32::from_le_bytes(d[12..16].try_into().unwrap()), 2);
        assert_eq!(&d[0x60..], &[10, 0, 0, 0, 12, 0, 0, 0]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 가짜 아이팟: 같은 곡 두 개 → 하나 빼기 → 되돌리기
    #[test]
    fn remove_then_restore() {
        let mount = std::env::temp_dir().join(format!("podlift-dd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&mount);
        let f00 = mount.join("iPod_Control/Music/F00");
        std::fs::create_dir_all(&f00).unwrap();
        std::fs::create_dir_all(mount.join("iPod_Control/iTunes")).unwrap();
        let mut db = ITunesDb::parse(&empty_db(false)).unwrap();
        let mut ids = Vec::new();
        for name in ["AAAA", "BBBB"] {
            std::fs::write(f00.join(format!("{name}.mp3")), name).unwrap();
            ids.push(
                db.add_track(&NewTrack {
                    location: format!(":iPod_Control:Music:F00:{name}.mp3"),
                    ext: "mp3".into(),
                    title: "Song".into(),
                    album: "Album".into(),
                    ..Default::default()
                })
                .unwrap(),
            );
        }
        std::fs::write(f00.join("CCCC.mp3"), "orphan").unwrap();
        sync::write_db(&mount, &mut db, None).unwrap();
        let before = std::fs::read(sync::db_path(&mount)).unwrap();

        let item = |n: &str| AudioInfo { path: f00.join(format!("{n}.mp3")), ..Default::default() };
        let g = crate::dedupe::Group {
            key: "k".into(),
            album: "Album".into(),
            title: "Song".into(),
            items: vec![item("AAAA"), item("BBBB"), item("CCCC")],
            keep: 1,
            verdict: crate::audio::Verdict::Same,
            length_mismatch: false,
        };
        let tracks = sync::open_db(&mount).unwrap().tracks();
        // 고른 곡 지우기 계획
        let del = plan_delete(&mount, &tracks, &[f00.join("BBBB.mp3"), f00.join("CCCC.mp3")]);
        assert_eq!(del.remove.iter().map(|r| (r.0, r.1)).collect::<Vec<_>>(), vec![(ids[1], None)]);
        assert_eq!(del.orphans.len(), 1);

        // 여러 개 남기기·모두 지우기 계획
        let many = plan(&mount, &tracks, &[(g.clone(), Some(vec![true, true, false]))]);
        assert_eq!((many.remove.len(), many.orphans.len()), (0, 1));
        let all = plan(&mount, &tracks, &[(g.clone(), Some(vec![false, false, false]))]);
        assert_eq!(all.remove.iter().map(|r| (r.0, r.1)).collect::<Vec<_>>(), vec![(ids[0], None), (ids[1], None)]);

        let p = plan(&mount, &tracks, &[(g, Some(vec![false, true, false]))]);
        assert_eq!(p.remove.len(), 1);
        assert_eq!(p.remove[0].0, ids[0]);
        assert_eq!(p.orphans.len(), 1);

        let out = run(&mount, &p, None).unwrap();
        assert_eq!((out.removed, out.moved), (1, 2));
        assert!(!f00.join("AAAA.mp3").exists());
        assert!(mount.join("iPod_Control/Music/.podlift-old/F00/AAAA.mp3").exists());
        let after = sync::open_db(&mount).unwrap().tracks();
        assert_eq!(after.iter().map(|t| t.id).collect::<Vec<_>>(), vec![ids[1]]);

        // 다른 프로그램이 DB 를 바꾸면 되돌리지 않는다
        let current = std::fs::read(sync::db_path(&mount)).unwrap();
        std::fs::write(sync::db_path(&mount), &before).unwrap();
        assert!(sync::last_backup(&mount).is_none());
        assert!(sync::restore_last(&mount).is_err());
        std::fs::write(sync::db_path(&mount), &current).unwrap();
        assert!(sync::last_backup(&mount).is_some());
        sync::restore_last(&mount).unwrap();
        assert!(f00.join("AAAA.mp3").exists() && f00.join("CCCC.mp3").exists());
        assert_eq!(std::fs::read(sync::db_path(&mount)).unwrap(), before);
        let _ = std::fs::remove_dir_all(&mount);
    }
}
