//! 실제 아이팟 읽기 점검 (아이팟에는 아무것도 쓰지 않는다). PODLIFT_REAL_IPOD=<마운트> 일 때만 돈다.
//! PODLIFT_REAL_DEDUPE 가 있으면 중복 찾기와 중복 정리 모의 실행(메모리 안에서만)까지 한다.
//! PODLIFT_REAL_OUT=<폴더> 이면 중복 목록과 모의 실행 결과 DB 를 그 폴더에 남긴다.
use crate::{artworkdb::ArtworkDb, audio::Verdict, dedupe, ipod, ipod_dedupe, itunesdb::ITunesDb, sync};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

fn report_indexes(out: &mut String, label: &str, db: &ITunesDb) {
    let r = db.index_report();
    let _ = writeln!(out, "[{label}] 기본 재생 목록 곡 {}개, 끊긴 재생 목록 항목 {}개", r.members, r.dangling);
    for (sort, n, bad, jumps) in r.sorts {
        let _ = writeln!(out, "    정렬 {sort:#04x}: 항목 {n}, 우리 규칙과 어긋난 이웃 {bad}, 건너뛰기 표 합 {jumps:?}");
    }
}

#[test]
fn real_ipod_read_check() {
    let Ok(mount) = std::env::var("PODLIFT_REAL_IPOD") else { return };
    let mount = PathBuf::from(mount);
    let out_dir = std::env::var("PODLIFT_REAL_OUT").ok().map(PathBuf::from);
    let mut out = String::new();

    let found = ipod::detect();
    let _ = writeln!(out, "[연결] detect: {:?}", found.iter().map(|i| (&i.name, &i.model)).collect::<Vec<_>>());
    let fw = ipod::firewire_id(&mount);
    let _ = writeln!(out, "[연결] FirewireGuid: {fw:?}");
    let _ = writeln!(out, "[연결] 남은 공간: {:?}", ipod::free_space(&mount).map(crate::app::human_bytes));
    let fwid = fw.and_then(|s| crate::itunesdb::parse_firewire_id(&s));

    let raw = std::fs::read(sync::db_path(&mount)).unwrap();
    let mut db = sync::open_db(&mount).expect("곡 DB 열기");
    let tracks = db.tracks();
    let _ = writeln!(out, "[곡 DB] {} 바이트, 읽기·되쓰기 확인 통과, 서명 방식 {}, 곡 {}개", raw.len(), db.hashing_scheme(), tracks.len());
    let missing = tracks.iter().filter(|t| !t.file_path(&mount).is_some_and(|p| p.is_file())).count();
    let _ = writeln!(out, "[곡 DB] 파일 없는 곡 {missing}개, 앨범 아트 있는 곡 {}개", tracks.iter().filter(|t| t.has_artwork).count());
    if let Some(fwid) = &fwid {
        let mut copy = raw.clone();
        crate::itunesdb::write_hash58(&mut copy, fwid);
        let _ = writeln!(out, "[곡 DB] 서명 다시 계산: {}", if copy == raw { "기존 서명과 같음" } else { "기존 서명과 다름" });
        let again = db.to_bytes(Some(fwid)).unwrap();
        let _ = writeln!(out, "[곡 DB] 고치지 않고 다시 쓰기: {}", if again == raw { "원본과 바이트까지 같음" } else { "원본과 다름" });
    }
    report_indexes(&mut out, "Apple 색인", &db);

    // 곡 하나를 빼면 나머지 순서는 Apple 순서 그대로여야 한다
    let apple = db.index_orders();
    let (gone, keep) = (tracks[10].id, tracks[11].id);
    let mut one = ITunesDb::parse(&raw).unwrap();
    one.remove_tracks(&[(gone, Some(keep))]).unwrap();
    let bytes = one.to_bytes(fwid.as_deref()).unwrap();
    let one = ITunesDb::parse(&bytes).unwrap();
    let after = one.index_orders();
    let same = apple.iter().all(|(sort, ids)| {
        let expect: Vec<u32> = ids.iter().copied().filter(|&id| id != gone).collect();
        after.get(sort) == Some(&expect)
    });
    let _ = writeln!(out, "[곡 하나 빼기] 남은 곡의 순서가 Apple 순서 그대로: {same}");
    if !same {
        for (sort, ids) in &apple {
            let expect: Vec<u32> = ids.iter().copied().filter(|&id| id != gone).collect();
            let got = after.get(sort).cloned().unwrap_or_default();
            let at = expect.iter().zip(&got).position(|(a, b)| a != b);
            let _ = writeln!(out, "    정렬 {sort:#04x}: 기대 {} 개, 실제 {} 개, 처음 다른 곳 {at:?}", expect.len(), got.len());
        }
        let _ = writeln!(out, "    뺀 곡 {gone}, 남길 곡 {keep}, 곡 수 {}", one.tracks().len());
    }

    match ArtworkDb::open(&mount) {
        Ok(art) => {
            let _ = writeln!(out, "[그림 DB] 읽기·되쓰기 확인 통과, 형식 {:?}", art.formats().iter().map(|f| (f.id, f.width, f.stride)).collect::<Vec<_>>());
        }
        Err(e) => {
            let _ = writeln!(out, "[그림 DB] 열기 실패: {e}");
        }
    }

    if std::env::var("PODLIFT_REAL_ART").is_ok() {
        let needs = crate::ipod_art::needs(&mount, &tracks);
        let count = |f: &dyn Fn(&Option<crate::ipod_art::ArtSource>) -> bool| -> (usize, usize) {
            let v: Vec<_> = needs.iter().filter(|n| f(&n.source)).collect();
            (v.len(), v.iter().map(|n| n.track_ids.len()).sum())
        };
        use crate::ipod_art::ArtSource as A;
        let (ea, et) = count(&|s| matches!(s, Some(A::Existing(_))));
        let (fa, ft) = count(&|s| matches!(s, Some(A::Embedded(_))));
        let (na, nt) = count(&|s| s.is_none());
        let _ = writeln!(out, "[아트 채우기] 앨범 {}개: 아이팟 그림 같이 쓰기 {ea}앨범/{et}곡, 곡 파일 그림 {fa}앨범/{ft}곡, 그림 없음 {na}앨범/{nt}곡", needs.len());
    }

    if std::env::var("PODLIFT_REAL_DEDUPE").is_err() {
        println!("{out}");
        return;
    }

    // 중복 찾기 (중복 정리 탭과 같은 규칙)
    let stop = AtomicBool::new(false);
    let music = mount.join("iPod_Control").join("Music");
    let (groups, scanned) = dedupe::scan(&music, &stop, |_, _, _| {}).unwrap();
    let (mut q, mut c, mut same, mut u, mut len) = (0, 0, 0, 0, 0);
    for g in &groups {
        match g.verdict {
            Verdict::Quality(_) => q += 1,
            Verdict::Cover(_) => c += 1,
            Verdict::Same => same += 1,
            Verdict::Unsure => u += 1,
        }
        if g.length_mismatch {
            len += 1;
        }
    }
    let extra: usize = groups.iter().map(|g| g.items.len() - 1).sum();
    let _ = writeln!(out, "[중복] 훑은 파일 {scanned}개, 묶음 {}개, 남는 사본 {extra}개", groups.len());
    let _ = writeln!(out, "[중복] 판정: 음질 {q}, 앨범 아트 {c}, 같은 사본 {same}, 확인 필요 {u} (그중 길이 차이 큼 {len})");

    // 모의 실행: 확인 필요 묶음은 건드리지 않는다
    let chosen: Vec<(dedupe::Group, Option<Vec<bool>>)> = groups.iter().map(|g| (g.clone(), (g.verdict != Verdict::Unsure).then(|| ipod_dedupe::keep_only(g)))).collect();
    let plan = ipod_dedupe::plan(&mount, &tracks, &chosen);
    let bytes_freed: u64 = plan.remove.iter().map(|(_, _, p)| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)).sum::<u64>()
        + plan.orphans.iter().map(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)).sum::<u64>();
    let _ = writeln!(
        out,
        "[모의 실행] 뺄 곡 {}개, DB 에 없는 파일 {}개, 건너뛴 묶음 {}개, 비워질 용량 {}",
        plan.remove.len(),
        plan.orphans.len(),
        plan.skipped,
        crate::app::human_bytes(bytes_freed)
    );
    let mut sim = ITunesDb::parse(&raw).unwrap();
    let pairs: Vec<(u32, Option<u32>)> = plan.remove.iter().map(|(a, b, _)| (*a, *b)).collect();
    let pos = sim.remove_tracks(&pairs).unwrap();
    let bytes = sim.to_bytes(fwid.as_deref()).unwrap();
    let sim = ITunesDb::parse(&bytes).expect("모의 실행 DB 다시 읽기");
    let _ = writeln!(out, "[모의 실행] 새 DB {} 바이트, 곡 {}개 (뺀 위치 {}개)", bytes.len(), sim.tracks().len(), pos.len());
    report_indexes(&mut out, "모의 실행 색인", &sim);
    let kept_order = apple.iter().all(|(sort, ids)| {
        let gone: std::collections::HashSet<u32> = pairs.iter().map(|p| p.0).collect();
        let expect: Vec<u32> = ids.iter().copied().filter(|id| !gone.contains(id)).collect();
        sim.index_orders().get(sort) == Some(&expect)
    });
    let _ = writeln!(out, "[모의 실행] 남은 곡의 순서가 Apple 순서 그대로: {kept_order}");

    if let Some(dir) = &out_dir {
        let _ = std::fs::create_dir_all(dir);
        std::fs::write(dir.join("sim.itdb"), &bytes).unwrap();
        let mut list = String::new();
        for g in &groups {
            let v = match g.verdict {
                Verdict::Quality(_) => "음질",
                Verdict::Cover(_) => "아트",
                Verdict::Same => "같음",
                Verdict::Unsure if g.length_mismatch => "확인(길이)",
                Verdict::Unsure => "확인",
            };
            let _ = writeln!(list, "## [{v}] {} / {}", g.album, g.title);
            for (i, it) in g.items.iter().enumerate() {
                let mark = if g.verdict == Verdict::Unsure { "?" } else if i == g.keep { "남김" } else { "뺌" };
                let cover = it.cover.as_ref().map(|c| format!("{}x{}", c.width, c.height)).unwrap_or_else(|| "없음".into());
                let rel = it.path.strip_prefix(&mount).unwrap_or(&it.path).display().to_string();
                let _ = writeln!(list, "  {mark:4} {} | {} | {}:{:02} | 아트 {cover} | {} | {}", it.quality_label(), it.artist, it.duration_ms / 60000, it.duration_ms / 1000 % 60, crate::app::human_bytes(it.size), rel);
            }
        }
        std::fs::write(dir.join("ipod-dupes.txt"), format!("{out}\n{list}")).unwrap();
    }
    println!("{out}");
}

/// 실제 아이팟에서 중복 정리를 실행한다 (확인 필요 묶음은 건드리지 않음).
/// PODLIFT_REAL_IPOD=<마운트> 와 PODLIFT_REAL_APPLY=yes-write 가 모두 있을 때만 돈다.
#[test]
fn real_ipod_apply_dedupe() {
    let (Ok(mount), Ok(flag)) = (std::env::var("PODLIFT_REAL_IPOD"), std::env::var("PODLIFT_REAL_APPLY")) else { return };
    assert_eq!(flag, "yes-write");
    let mount = PathBuf::from(mount);
    let fwid = ipod::firewire_id(&mount).and_then(|s| crate::itunesdb::parse_firewire_id(&s)).expect("고유 번호");
    let tracks = sync::open_db(&mount).unwrap().tracks();
    let stop = AtomicBool::new(false);
    let (groups, _) = dedupe::scan(&mount.join("iPod_Control").join("Music"), &stop, |_, _, _| {}).unwrap();
    let chosen: Vec<(dedupe::Group, Option<Vec<bool>>)> = groups.iter().map(|g| (g.clone(), (g.verdict != Verdict::Unsure).then(|| ipod_dedupe::keep_only(g)))).collect();
    let plan = ipod_dedupe::plan(&mount, &tracks, &chosen);
    println!("[실행] 계획: 뺄 곡 {}개, DB 에 없는 파일 {}개, 건너뛴 묶음 {}", plan.remove.len(), plan.orphans.len(), plan.skipped);
    let outcome = ipod_dedupe::run(&mount, &plan, Some(&fwid)).expect("중복 정리 실행");
    println!("[실행] 결과: DB 에서 뺀 곡 {}, 옮긴 파일 {}, 실패 {:?}", outcome.removed, outcome.moved, outcome.failed);

    // 쓴 DB 를 다시 읽어 확인
    let raw = std::fs::read(sync::db_path(&mount)).unwrap();
    let mut copy = raw.clone();
    crate::itunesdb::write_hash58(&mut copy, &fwid);
    let db = sync::open_db(&mount).unwrap();
    let after = db.tracks();
    let missing = after.iter().filter(|t| !t.file_path(&mount).is_some_and(|p| p.is_file())).count();
    println!("[확인] 곡 {}개 (전 {}), 서명 {}, 파일 없는 곡 {missing}", after.len(), tracks.len(), if copy == raw { "맞음" } else { "틀림" });
    let mut out = String::new();
    report_indexes(&mut out, "확인 색인", &db);
    println!("{out}");
    println!("[확인] 되돌리기 기록: {:?}", sync::last_backup(&mount));
}
