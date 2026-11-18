//! 메인 화면. 머리 아래 분절 탭으로 네 화면을 오간다.
//! - 가져오기(이 파일): 아이팟 고르기 → 저장 폴더 → 곡 목록(남는 높이 전부) → 가져오기.
//!   아이팟을 고르면 곡 목록을 바로 읽는다. 연결된 아이팟은 2초마다 다시 찾는다.
//! - 아이팟에 넣기: sync_ui.rs
//! - 중복 정리: dedupe_ui.rs
//! - 폴더 정리: organize_ui.rs
//! 어느 탭이 보이든 각 탭의 백그라운드 작업은 매 프레임 pump 로 진행 상황을 받는다.
use crate::config::Config;
use crate::copier::{self, CopyEvent, Copier, Job, Outcome};
use crate::egui;
use crate::ipod::{self, Ipod};
use crate::library::{self, ScanEvent, Scanner, Track};
use crate::theme::{self, Palette};
use egui::RichText;
use egui_extras::{Column, TableBuilder};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

/// 왼쪽 위 신호등 단추 자리를 비워 두는 높이
const TOP_INSET: f32 = 30.0;
const DETECT_EVERY: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Import,
    Sync,
    Dedupe,
    Organize,
}

impl Tab {
    const ALL: [(Tab, &'static str); 4] = [(Tab::Import, "가져오기"), (Tab::Sync, "아이팟에 넣기"), (Tab::Dedupe, "중복 정리"), (Tab::Organize, "폴더 정리")];
}

#[derive(Clone, Copy, PartialEq)]
enum RowStatus {
    Copying,
    Copied,
    Skipped,
    Failed,
}

#[derive(Default)]
struct CopyStats {
    total_files: usize,
    total_bytes: u64,
    done_files: usize,
    done_bytes: u64,
    copied: usize,
    skipped: usize,
    /// (곡 위치, 오류 문구)
    failed: Vec<(usize, String)>,
    current: Option<usize>,
    started: Option<Instant>,
    /// 끝났으면 Some(멈춘 이유 또는 None)
    finished: Option<Option<String>>,
}

pub struct PodliftApp {
    tab: Tab,
    sync: crate::sync_ui::SyncTab,
    dedupe: crate::dedupe_ui::DedupeTab,
    organize: crate::organize_ui::OrganizeTab,
    cfg: Config,
    cfg_dirty: bool,
    ipods: Vec<Ipod>,
    /// 직접 고른 폴더 (자동 탐지 목록과 따로 둔다)
    manual: Option<Ipod>,
    selected: Option<Ipod>,
    last_detect: Instant,
    scanner: Option<Scanner>,
    scan_progress: (usize, usize),
    tracks: Vec<Track>,
    other_files: usize,
    status: Vec<Option<RowStatus>>,
    filter: String,
    /// 검색어·영상 포함 설정을 반영한 곡 위치 목록 (바뀔 때만 다시 만든다)
    visible: Vec<usize>,
    visible_dirty: bool,
    copier: Option<Copier>,
    stats: CopyStats,
    eject_rx: Option<Receiver<Result<(), String>>>,
    /// (문구, 오류 여부)
    notice: Option<(String, bool)>,
    icon_tex: Option<egui::TextureHandle>,
    /// 삭제할 곡으로 고른 곡 (tracks 위치)
    marked: HashSet<usize>,
    /// 키보드 커서 (visible 안의 위치)
    cursor: Option<usize>,
    scroll_cursor: bool,
    /// 삭제 경고창을 띄웠는지
    confirm_delete: bool,
    delete_job: Option<DeleteJob>,
    /// 처음 한 번 모니터 높이에 맞춰 창을 세로로 늘렸는지
    sized: bool,
}

/// 가져오기 탭의 곡 삭제: 아이팟이면 곡 DB 에서 빼고(되돌리기 가능), 직접 고른 폴더면 휴지통으로
enum DeleteJob {
    Ipod(crate::ipod_dedupe::Job),
    Folder(Receiver<(usize, Vec<String>)>),
}

impl PodliftApp {
    pub fn new() -> Self {
        let mut app = Self {
            tab: Tab::Import,
            sync: crate::sync_ui::SyncTab::new(),
            dedupe: crate::dedupe_ui::DedupeTab::new(),
            organize: crate::organize_ui::OrganizeTab::new(),
            cfg: Config::load(),
            cfg_dirty: false,
            ipods: Vec::new(),
            manual: None,
            selected: None,
            last_detect: Instant::now() - DETECT_EVERY,
            scanner: None,
            scan_progress: (0, 0),
            tracks: Vec::new(),
            other_files: 0,
            status: Vec::new(),
            filter: String::new(),
            visible: Vec::new(),
            visible_dirty: false,
            copier: None,
            stats: CopyStats::default(),
            eject_rx: None,
            notice: None,
            icon_tex: None,
            marked: HashSet::new(),
            cursor: None,
            scroll_cursor: false,
            confirm_delete: false,
            delete_job: None,
            sized: false,
        };
        // 개발·점검용: PODLIFT_TAB=sync|dedupe 로 시작 탭을 고른다.
        app.tab = match std::env::var("PODLIFT_TAB").as_deref() {
            Ok("sync") => Tab::Sync,
            Ok("dedupe") => Tab::Dedupe,
            Ok("organize") => Tab::Organize,
            _ => Tab::Import,
        };
        // 개발·점검용: PODLIFT_OPEN=<폴더> 로 실행하면 그 폴더를 직접 고른 것처럼 연다.
        if let Some(found) = std::env::var_os("PODLIFT_OPEN").and_then(|d| ipod::from_folder(Path::new(&d))) {
            app.manual = Some(found.clone());
            app.select(Some(found));
        }
        app.detect();
        app
    }

    fn is_scanning(&self) -> bool {
        self.scanner.is_some()
    }

    fn is_copying(&self) -> bool {
        self.copier.is_some()
    }

    fn is_busy(&self) -> bool {
        self.is_scanning() || self.is_copying() || self.eject_rx.is_some() || self.delete_job.is_some()
    }

    /// 가져오기 탭 키보드: ↑↓ 곡 이동, 스페이스 고르기, ⌘A 전체 선택, Esc 전체 해제, Delete 고른 곡 삭제
    fn handle_import_keys(&mut self, ctx: &egui::Context) {
        if self.is_busy() || self.confirm_delete || self.visible.is_empty() {
            return;
        }
        let keys = crate::listkeys::take(ctx, false);
        if !keys.any() {
            return;
        }
        if keys.up || keys.down {
            self.cursor = crate::listkeys::step(self.cursor, self.visible.len(), &keys);
            self.scroll_cursor = true;
        }
        if keys.space {
            let c = self.cursor.unwrap_or(0).min(self.visible.len() - 1);
            self.cursor = Some(c);
            let i = self.visible[c];
            if !self.marked.remove(&i) {
                self.marked.insert(i);
            }
        }
        if keys.select_all {
            self.marked = self.visible.iter().copied().collect();
        }
        if keys.clear {
            self.marked.clear();
        }
        if keys.delete && !self.marked.is_empty() {
            self.confirm_delete = true;
        }
    }

    /// 고른 곡을 지운다 (경고창에서 '삭제'를 누른 뒤)
    fn start_delete(&mut self) {
        let Some(sel) = self.selected.clone() else { return };
        let mut idx: Vec<usize> = self.marked.iter().copied().collect();
        idx.sort_unstable();
        let paths: Vec<PathBuf> = idx.iter().filter_map(|&i| self.tracks.get(i)).map(|t| t.src.clone()).collect();
        if paths.is_empty() {
            return;
        }
        if sel.device && self.sync.is_busy() {
            self.notice = Some(("아이팟에 곡을 넣는 중이라 지금은 지울 수 없습니다.".into(), true));
            return;
        }
        self.notice = None;
        self.delete_job = Some(if sel.device {
            DeleteJob::Ipod(crate::ipod_dedupe::start_delete(sel.mount.clone(), paths))
        } else {
            let (tx, rx) = std::sync::mpsc::channel();
            let root = sel.music.clone();
            std::thread::spawn(move || {
                let mut failed = Vec::new();
                let mut moved = Vec::new();
                for p in &paths {
                    match trash::delete(p) {
                        Ok(()) => moved.push(p.clone()),
                        Err(e) => failed.push(format!("{}: {e}", p.display())),
                    }
                }
                crate::dedupe::prune_empty_dirs(&root, &moved);
                let _ = tx.send((moved.len(), failed));
            });
            DeleteJob::Folder(rx)
        });
    }

    /// 연결된 아이팟 목록을 새로 만든다. 고른 아이팟이 빠지면 선택을 지우고,
    /// 아무것도 고르지 않았는데 하나가 새로 보이면 그것을 고른다.
    fn detect(&mut self) {
        self.last_detect = Instant::now();
        let found = ipod::detect();
        if found == self.ipods {
            return;
        }
        self.ipods = found;
        if let Some(sel) = &self.selected {
            if sel.device && !self.ipods.iter().any(|i| i.mount == sel.mount) {
                self.select(None);
            }
        }
        if self.selected.is_none() {
            if let Some(first) = self.ipods.first().cloned() {
                self.select(Some(first));
            }
        }
    }

    fn select(&mut self, ipod: Option<Ipod>) {
        if let Some(s) = self.scanner.take() {
            s.cancel();
        }
        self.tracks.clear();
        self.status.clear();
        self.visible.clear();
        self.stats = CopyStats::default();
        self.selected = ipod;
        if self.selected.is_some() {
            self.start_scan();
        }
    }

    fn start_scan(&mut self) {
        let Some(sel) = &self.selected else { return };
        self.marked.clear();
        self.cursor = None;
        self.scan_progress = (0, 0);
        self.tracks.clear();
        self.status.clear();
        self.visible.clear();
        self.stats = CopyStats::default();
        self.scanner = Some(library::start_scan(sel.music.clone()));
    }

    fn pick_folder(&mut self) {
        let Some(dir) = rfd::FileDialog::new().set_title("아이팟 또는 아이팟 백업 폴더 선택").pick_folder() else { return };
        match ipod::from_folder(&dir) {
            Some(found) => {
                self.manual = Some(found.clone());
                self.select(Some(found));
            }
            None => {
                self.notice = Some((
                    "이 폴더에서 아이팟 음악 폴더(iPod_Control/Music/F00…)를 찾지 못했습니다.".into(),
                    true,
                ))
            }
        }
    }

    fn rebuild_visible(&mut self) {
        let q = self.filter.trim().to_lowercase();
        let words: Vec<&str> = q.split_whitespace().collect();
        self.visible = self
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, t)| self.cfg.include_video || !t.video)
            .filter(|(_, t)| words.iter().all(|w| t.search_key.contains(w)))
            .map(|(i, _)| i)
            .collect();
        self.visible_dirty = false;
    }

    fn start_copy(&mut self) {
        let Some(sel) = &self.selected else { return };
        let out = PathBuf::from(self.cfg.output_path.trim());
        if self.cfg.output_path.trim().is_empty() {
            self.notice = Some(("저장 폴더를 먼저 정하세요.".into(), true));
            return;
        }
        if let Err(e) = std::fs::create_dir_all(&out) {
            self.notice = Some((format!("저장 폴더를 만들지 못했습니다: {e}"), true));
            return;
        }
        let jobs: Vec<Job> = self
            .visible
            .iter()
            .map(|&i| {
                let t = &self.tracks[i];
                Job { index: i, src: t.src.clone(), dest: out.join(&t.rel_dest), size: t.size }
            })
            .collect();
        self.status = vec![None; self.tracks.len()];
        self.stats = CopyStats {
            total_files: jobs.len(),
            total_bytes: jobs.iter().map(|j| j.size).sum(),
            started: Some(Instant::now()),
            ..Default::default()
        };
        self.notice = None;
        self.copier = Some(copier::start(jobs, sel.music.clone()));
    }

    fn start_eject(&mut self) {
        let Some(sel) = self.selected.clone().filter(|s| s.device) else { return };
        if self.sync.is_busy() {
            self.notice = Some(("아이팟에 곡을 넣는 중이라 꺼낼 수 없습니다. 끝난 뒤에 꺼내세요.".into(), true));
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(ipod::eject(&sel.mount));
        });
        self.eject_rx = Some(rx);
    }

    fn pump(&mut self) {
        if let Some(scanner) = &self.scanner {
            for ev in scanner.poll() {
                match ev {
                    ScanEvent::Progress(done, total) => self.scan_progress = (done, total),
                    ScanEvent::Done(tracks, others) => {
                        self.status = vec![None; tracks.len()];
                        self.tracks = tracks;
                        self.other_files = others;
                        self.visible_dirty = true;
                        self.scanner = None;
                        break;
                    }
                    ScanEvent::Failed(msg) => {
                        self.notice = Some((msg, true));
                        self.scanner = None;
                        break;
                    }
                }
            }
        }
        if let Some(copier) = &self.copier {
            for ev in copier.poll() {
                match ev {
                    CopyEvent::Begin(i) => {
                        self.stats.current = Some(i);
                        self.status[i] = Some(RowStatus::Copying);
                    }
                    CopyEvent::Done(i, size, outcome) => {
                        self.stats.done_files += 1;
                        self.stats.done_bytes += size;
                        self.status[i] = Some(match outcome {
                            Outcome::Copied => {
                                self.stats.copied += 1;
                                RowStatus::Copied
                            }
                            Outcome::Skipped => {
                                self.stats.skipped += 1;
                                RowStatus::Skipped
                            }
                            Outcome::Failed(msg) => {
                                self.stats.failed.push((i, msg));
                                RowStatus::Failed
                            }
                        });
                    }
                    CopyEvent::Finished(reason) => {
                        self.stats.current = None;
                        self.stats.finished = Some(reason);
                        self.copier = None;
                        break;
                    }
                }
            }
        }
        let deleted = match &self.delete_job {
            Some(DeleteJob::Ipod(job)) => job.poll().map(|r| match r {
                Ok(out) => {
                    let mut t = format!("아이팟에서 {}곡을 지웠습니다. 파일은 아이팟 안 .podlift-old 에 옮겨 두었고, '아이팟에 넣기' 탭의 '마지막 작업 되돌리기'로 되돌릴 수 있습니다.", out.removed + out.moved.saturating_sub(out.removed));
                    if !out.failed.is_empty() {
                        t.push_str(&format!(" 파일 {}개는 옮기지 못했습니다.", out.failed.len()));
                    }
                    (t, !out.failed.is_empty())
                }
                Err(e) => (format!("지우지 못했습니다: {e}"), true),
            }),
            Some(DeleteJob::Folder(rx)) => rx.try_recv().ok().map(|(n, failed)| match failed.first() {
                None => (format!("{n}개 파일을 휴지통으로 옮겼습니다."), false),
                Some(f) => (format!("{n}개는 휴지통으로 옮겼고 {}개는 실패했습니다 ({f}).", failed.len()), true),
            }),
            None => None,
        };
        if let Some(notice) = deleted {
            let ipod = matches!(self.delete_job, Some(DeleteJob::Ipod(_)));
            self.delete_job = None;
            self.notice = Some(notice);
            self.marked.clear();
            self.cursor = None;
            if ipod {
                self.sync.reload();
            }
            self.start_scan();
        }
        if let Some(rx) = &self.eject_rx {
            if let Ok(result) = rx.try_recv() {
                self.eject_rx = None;
                self.notice = Some(match result {
                    Ok(()) => ("아이팟을 꺼냈습니다. 이제 케이블을 뽑아도 됩니다.".into(), false),
                    Err(e) => (format!("꺼내지 못했습니다: {e}"), true),
                });
                self.detect();
            }
        }
    }

    // ── 화면 조각 ──────────────────────────────────────────

    fn header(ui: &mut egui::Ui, p: &Palette, title: &str) {
        ui.label(RichText::new(title).font(theme::bold(16.0)).color(p.text));
    }

    fn help(ui: &mut egui::Ui, p: &Palette, text: &str) {
        ui.label(RichText::new(text).size(13.5).color(p.muted));
    }

    fn ipod_card(&mut self, ui: &mut egui::Ui, p: &Palette) {
        ui.horizontal(|ui| {
            Self::header(ui, p, "아이팟");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_enabled_ui(!self.is_busy(), |ui| {
                    if ui.button("폴더 직접 선택…").on_hover_text("아이팟을 통째로 복사해 둔 백업 폴더도 고를 수 있습니다").clicked() {
                        self.pick_folder();
                    }
                });
            });
        });
        let mut choices: Vec<Ipod> = self.ipods.clone();
        if let Some(m) = &self.manual {
            choices.push(m.clone());
        }
        if choices.is_empty() {
            Self::help(ui, p, "연결된 아이팟이 없습니다. 케이블로 연결하면 저절로 나타납니다.");
            Self::help(
                ui,
                p,
                "Finder 사이드바에 아이팟이 보이지 않으면, 아이팟에서 '디스크 사용'이 켜져 있는지 확인하세요. 화면이 멈춰 있으면 메뉴+가운데 버튼을 길게 눌러 다시 켠 뒤, 사과 로고가 뜨면 선택+재생 버튼을 눌러 디스크 모드로 들어갈 수 있습니다.",
            );
            return;
        }
        let mut pick = None;
        ui.horizontal_wrapped(|ui| {
            for ipod in &choices {
                let on = self.selected.as_ref().is_some_and(|s| s.mount == ipod.mount && s.music == ipod.music);
                let mut label = if ipod.device { ipod.name.clone() } else { format!("{} (폴더)", ipod.name) };
                if !ipod.model.is_empty() {
                    label.push_str(&format!("  ·  {}", ipod.model));
                }
                let text = if on {
                    RichText::new(label).font(theme::bold(14.0)).color(p.accent_text)
                } else {
                    RichText::new(label).color(p.text)
                };
                let btn = egui::Button::new(text)
                    .fill(if on { p.accent } else { p.card_strong })
                    .stroke(egui::Stroke::new(1.0, if on { p.accent } else { p.border }))
                    .corner_radius(10);
                let resp = ui.add_enabled(!self.is_busy() || on, btn).on_hover_text(ipod.mount.display().to_string());
                if resp.clicked() && !on {
                    pick = Some(ipod.clone());
                }
            }
        });
        if let Some(i) = pick {
            self.select(Some(i));
        }
    }

    fn output_card(&mut self, ui: &mut egui::Ui, p: &Palette) {
        Self::header(ui, p, "저장 폴더");
        Self::help(ui, p, "이 폴더 안에 '가수/앨범' 하위 폴더가 만들어집니다. 같은 곡이 이미 있으면 건너뛰므로, 중간에 멈춰도 다시 시작하면 이어서 가져옵니다.");
        ui.add_enabled_ui(!self.is_copying(), |ui| {
            ui.horizontal(|ui| {
                let r = ui.add(egui::TextEdit::singleline(&mut self.cfg.output_path).desired_width(ui.available_width() - 170.0));
                if r.changed() {
                    self.cfg_dirty = true;
                }
                if ui.button("폴더 선택…").clicked() {
                    if let Some(d) = rfd::FileDialog::new().set_title("저장 폴더 선택").pick_folder() {
                        self.cfg.output_path = d.to_string_lossy().into_owned();
                        self.cfg_dirty = true;
                    }
                }
                if ui.button("열기").clicked() {
                    let _ = std::fs::create_dir_all(self.cfg.output_path.trim());
                    open_path(Path::new(self.cfg.output_path.trim()));
                }
            });
        });
    }

    fn library_card(&mut self, ui: &mut egui::Ui, p: &Palette) {
        if self.selected.is_none() {
            Self::header(ui, p, "곡 목록");
            Self::help(ui, p, "아이팟을 고르면 곡 목록을 읽습니다.");
            return;
        }
        if self.is_scanning() {
            Self::header(ui, p, "곡 목록 읽는 중…");
            let (done, total) = self.scan_progress;
            let frac = if total == 0 { 0.0 } else { done as f32 / total as f32 };
            ui.add(egui::ProgressBar::new(frac).text(if total == 0 { "파일 찾는 중…".to_string() } else { format!("{done} / {total}") }));
            Self::help(ui, p, "아이팟 하드디스크는 느려서 곡이 많으면 몇 분 걸릴 수 있습니다.");
            return;
        }

        // 요약 칩
        let shown: Vec<&Track> = self.tracks.iter().filter(|t| self.cfg.include_video || !t.video).collect();
        let bytes: u64 = shown.iter().map(|t| t.size).sum();
        let albums: std::collections::HashSet<(String, &str)> =
            shown.iter().map(|t| (library::artist_folder(t).to_lowercase(), t.album.as_str())).collect();
        let protected = shown.iter().filter(|t| t.protected).count();
        let untagged = shown.iter().filter(|t| !t.tagged).count();
        let videos = self.tracks.iter().filter(|t| t.video).count();
        ui.horizontal_wrapped(|ui| {
            Self::header(ui, p, "곡 목록");
            theme::chip(ui, &format!("{}곡", shown.len()), p.accent, true);
            theme::chip(ui, &human_bytes(bytes), p.blue, false);
            theme::chip(ui, &format!("앨범 {}", albums.len()), p.teal, false);
            if protected > 0 {
                theme::chip(ui, &format!("보호된 곡 {protected}"), p.amber, true).on_hover_text(
                    "iTunes Store 에서 2009년 이전에 산 .m4p 곡입니다. 복사는 되지만 구매한 Apple ID 로 인증한 컴퓨터에서만 재생됩니다.",
                );
            }
            if untagged > 0 {
                theme::chip(ui, &format!("태그 없음 {untagged}"), p.yellow, false)
                    .on_hover_text("제목 태그가 없어 아이팟 안의 파일 이름(예: ABCD)으로 저장됩니다. 주로 WAV 파일입니다.");
            }
            if self.other_files > 0 {
                theme::chip(ui, &format!("그 밖의 파일 {}", self.other_files), p.muted, false)
                    .on_hover_text("음악·영상이 아닌 파일이라 가져오지 않습니다.");
            }
        });
        ui.horizontal(|ui| {
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text("제목·가수·앨범 검색 (검색하면 찾은 곡만 가져옵니다)")
                    .desired_width(ui.available_width() - 230.0),
            );
            if r.changed() {
                self.visible_dirty = true;
            }
            ui.add_enabled_ui(!self.is_copying(), |ui| {
                let label = if videos > 0 { format!("영상도 포함 ({videos})") } else { "영상도 포함".to_string() };
                if ui.checkbox(&mut self.cfg.include_video, label).changed() {
                    self.cfg_dirty = true;
                    self.visible_dirty = true;
                }
                if ui.button("다시 읽기").clicked() {
                    self.start_scan();
                }
            });
        });
        if self.visible_dirty {
            self.rebuild_visible();
        }
        self.table(ui, p);
    }

    fn table(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let row_h = 24.0;
        let follow = self.stats.current.and_then(|c| self.visible.iter().position(|&i| i == c));
        let follow = if std::mem::take(&mut self.scroll_cursor) { self.cursor.or(follow) } else { follow };
        let mut table = TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .sense(egui::Sense::click())
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(44.0))
            .column(Column::exact(26.0))
            .column(Column::initial(240.0).at_least(120.0).clip(true))
            .column(Column::initial(170.0).at_least(80.0).clip(true))
            .column(Column::initial(190.0).at_least(80.0).clip(true))
            .column(Column::remainder().at_least(120.0).clip(true))
            .min_scrolled_height(0.0)
            .max_scroll_height(f32::INFINITY);
        if let Some(row) = follow {
            table = table.scroll_to_row(row, Some(egui::Align::Center));
        }
        let muted = p.muted;
        table
            .header(22.0, |mut h| {
                for title in ["선택", "", "제목", "가수", "앨범", "저장될 위치"] {
                    h.col(|ui| {
                        ui.label(RichText::new(title).font(theme::bold(13.0)).color(muted));
                    });
                }
            })
            .body(|body| {
                let marked = &mut self.marked;
                let cursor = &mut self.cursor;
                body.rows(row_h, self.visible.len(), |mut row| {
                    let pos = row.index();
                    let i = self.visible[pos];
                    let t = &self.tracks[i];
                    let on = marked.contains(&i);
                    row.set_selected(on);
                    row.col(|ui| {
                        let here = *cursor == Some(pos);
                        ui.label(RichText::new(if here { "→" } else { " " }).font(theme::bold(13.0)).color(p.accent));
                        crate::dedupe_ui::keep_mark(ui, p, true, on);
                    });
                    row.col(|ui| {
                        let (mark, color, tip) = match self.status.get(i).copied().flatten() {
                            Some(RowStatus::Copying) => ("…", p.blue, "복사 중"),
                            Some(RowStatus::Copied) => ("✔", p.green, "복사함"),
                            Some(RowStatus::Skipped) => ("=", p.teal, "이미 있어 건너뜀"),
                            Some(RowStatus::Failed) => ("✖", p.red, "실패"),
                            None => ("", p.muted, ""),
                        };
                        let r = ui.label(RichText::new(mark).color(color));
                        if !tip.is_empty() {
                            r.on_hover_text(tip);
                        }
                    });
                    row.col(|ui| {
                        if t.protected {
                            ui.label(RichText::new("DRM").size(11.0).color(p.amber));
                        }
                        let color = if t.tagged { p.text } else { p.yellow };
                        ui.add(egui::Label::new(RichText::new(&t.title).color(color)).truncate());
                    });
                    row.col(|ui| {
                        ui.add(egui::Label::new(RichText::new(&t.artist).color(p.text)).truncate());
                    });
                    row.col(|ui| {
                        ui.add(egui::Label::new(RichText::new(&t.album).color(p.text)).truncate());
                    });
                    row.col(|ui| {
                        ui.add(egui::Label::new(RichText::new(t.rel_dest.to_string_lossy()).size(12.5).color(muted)).truncate());
                    });
                    // 줄 어디를 눌러도 고르기를 켜고 끈다
                    if row.response().clicked() {
                        if !marked.remove(&i) {
                            marked.insert(i);
                        }
                        *cursor = Some(pos);
                    }
                });
            });
    }

    fn action_bar(&mut self, ui: &mut egui::Ui, p: &Palette) {
        if self.is_copying() {
            let s = &self.stats;
            let frac = if s.total_bytes == 0 { 0.0 } else { s.done_bytes as f32 / s.total_bytes as f32 };
            let mut text = format!(
                "{} / {}곡  ·  {} / {}",
                s.done_files,
                s.total_files,
                human_bytes(s.done_bytes),
                human_bytes(s.total_bytes)
            );
            if let Some(eta) = eta(s) {
                text.push_str(&format!("  ·  남은 시간 약 {eta}"));
            }
            ui.add(egui::ProgressBar::new(frac).text(text));
            ui.horizontal(|ui| {
                if let Some(t) = s.current.and_then(|i| self.tracks.get(i)) {
                    ui.add(egui::Label::new(RichText::new(format!("지금: {} - {}", t.artist, t.title)).color(p.muted)).truncate());
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::tinted_button(ui, p.red, "중지", egui::vec2(110.0, 36.0)).clicked() {
                        if let Some(c) = &self.copier {
                            c.cancel();
                        }
                    }
                });
            });
            return;
        }

        if let Some(reason) = self.stats.finished.clone() {
            let s = &self.stats;
            let color = if !s.failed.is_empty() || reason.is_some() { p.amber } else { p.green };
            theme::banner(p, color).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    let head = match &reason {
                        Some(r) => r.clone(),
                        None => "가져오기를 마쳤습니다.".into(),
                    };
                    ui.label(RichText::new(head).font(theme::bold(14.0)).color(p.text));
                    theme::chip(ui, &format!("새로 복사 {}", s.copied), p.green, true);
                    theme::chip(ui, &format!("이미 있어 건너뜀 {}", s.skipped), p.teal, false);
                    if !s.failed.is_empty() {
                        theme::chip(ui, &format!("실패 {}", s.failed.len()), p.red, true);
                    }
                });
                if !s.failed.is_empty() {
                    egui::CollapsingHeader::new(RichText::new("실패한 곡 보기").color(p.text)).show(ui, |ui| {
                        egui::ScrollArea::vertical().max_height(140.0).show(ui, |ui| {
                            for (i, msg) in &s.failed {
                                let name = self.tracks.get(*i).map(|t| t.rel_dest.to_string_lossy().into_owned()).unwrap_or_default();
                                ui.label(RichText::new(format!("{name}  -  {msg}")).size(12.5).color(p.red));
                            }
                        });
                    });
                }
            });
        }

        ui.horizontal(|ui| {
            let n = self.visible.len();
            let ready = self.selected.is_some() && !self.is_scanning() && n > 0 && self.eject_rx.is_none();
            let label = if self.filter.trim().is_empty() { format!("전체 {n}곡 가져오기") } else { format!("찾은 {n}곡 가져오기") };
            if theme::primary_button(ui, p, &label, ready).clicked() {
                self.start_copy();
            }
            if self.stats.finished.is_some() && ui.button("저장 폴더 열기").clicked() {
                open_path(Path::new(self.cfg.output_path.trim()));
            }
            if !self.visible.is_empty() {
                ui.add_space(10.0);
                ui.add_enabled_ui(!self.is_busy(), |ui| {
                    if ui.button("전체 선택").on_hover_text("⌘A").clicked() {
                        self.marked = self.visible.iter().copied().collect();
                    }
                    if ui.button("전체 해제").on_hover_text("Esc").clicked() {
                        self.marked.clear();
                    }
                    let n = self.marked.len();
                    let r = theme::tinted_button(ui, p.red, &format!("선택한 {n}곡 삭제"), egui::vec2(150.0, 36.0));
                    if r.on_hover_text("Delete 키. 누르면 한 번 더 묻습니다.").clicked() && n > 0 {
                        self.confirm_delete = true;
                    }
                });
                if self.delete_job.is_some() {
                    ui.spinner();
                    ui.label(RichText::new("지우는 중…").color(p.muted));
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.selected.as_ref().is_some_and(|s| s.device) {
                    let ejecting = self.eject_rx.is_some();
                    let label = if ejecting { "꺼내는 중…" } else { "아이팟 꺼내기" };
                    ui.add_enabled_ui(!self.is_scanning() && !ejecting, |ui| {
                        if theme::ghost_button(ui, p, label).clicked() {
                            self.start_eject();
                        }
                    });
                }
            });
        });
    }
}

pub fn human_bytes(b: u64) -> String {
    let b = b as f64;
    if b >= 1e9 {
        format!("{:.1} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.0} MB", b / 1e6)
    } else {
        format!("{:.0} KB", b / 1e3)
    }
}

/// 지금까지의 속도로 남은 시간을 어림한다. 너무 이르면 None
fn eta(s: &CopyStats) -> Option<String> {
    let secs = s.started?.elapsed().as_secs_f64();
    if secs < 5.0 || s.done_bytes == 0 {
        return None;
    }
    let left = (s.total_bytes - s.done_bytes) as f64 / (s.done_bytes as f64 / secs);
    Some(if left < 60.0 { "1분 이내".into() } else if left < 3600.0 { format!("{:.0}분", left / 60.0) } else { format!("{:.1}시간", left / 3600.0) })
}

pub fn open_path(p: &Path) {
    let _ = std::process::Command::new("open").arg(p).spawn();
}

impl eframe::App for PodliftApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // 처음 한 번: 모니터에 맞춰 창을 세로로 길게 (중복 묶음 하나가 한 화면에 들어오게)
        if !self.sized {
            if let Some(m) = ctx.input(|i| i.viewport().monitor_size) {
                self.sized = true;
                let h = (m.y - 70.0).clamp(860.0, 1200.0);
                let w = 1040.0_f32.min(m.x - 40.0);
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(w, h)));
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(((m.x - w) / 2.0).max(0.0), 30.0)));
            }
        }
        self.pump();
        self.sync.pump();
        self.dedupe.pump();
        self.organize.pump();
        if self.dedupe.take_ipod_changed() {
            self.sync.reload();
        }
        if !self.is_busy() && self.last_detect.elapsed() >= DETECT_EVERY {
            self.detect();
        }
        if self.is_busy() || self.sync.is_busy() || self.dedupe.is_busy() || self.organize.is_busy() {
            ctx.request_repaint_after(Duration::from_millis(100));
        } else {
            ctx.request_repaint_after(DETECT_EVERY);
        }
        let p = theme::palette_for(ui);
        theme::backdrop(&ctx.layer_painter(egui::LayerId::background()), ctx.viewport_rect(), &p);

        if self.icon_tex.is_none() {
            if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../icon.png")) {
                let img = egui::ColorImage::from_rgba_unmultiplied([icon.width as usize, icon.height as usize], &icon.rgba);
                self.icon_tex = Some(ctx.load_texture("app-icon", img, egui::TextureOptions::LINEAR));
            }
        }

        let pad = egui::Frame::new().inner_margin(egui::Margin { left: 18, right: 18, top: 0, bottom: 10 });

        egui::Panel::top("top").frame(pad).show(ui, |ui| {
            ui.add_space(TOP_INSET);
            // 헤더 띠를 끌면 창이 움직인다 (제목 표시줄 숨김 보완)
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 52.0), egui::Sense::drag());
            if resp.dragged() {
                ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
            }
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(egui::Layout::left_to_right(egui::Align::Center)));
            if let Some(tex) = &self.icon_tex {
                child.add(egui::Image::from_texture((tex.id(), egui::vec2(44.0, 44.0))));
                child.add_space(4.0);
            }
            child.vertical(|ui| {
                ui.add_space(4.0);
                ui.label(RichText::new("Podlift").font(theme::bold(21.0)).color(p.text));
                ui.label(RichText::new("아이팟 클래식에서 음악을 가져오고, 넣고, 중복을 정리합니다.").size(13.0).color(p.muted));
            });
            ui.add_space(6.0);
            let current = Tab::ALL.iter().position(|(t, _)| *t == self.tab).unwrap_or(0);
            let labels: Vec<&str> = Tab::ALL.iter().map(|(_, l)| *l).collect();
            if let Some(i) = theme::segments(ui, &p, "main-tabs", &labels, current) {
                self.tab = Tab::ALL[i].0;
            }
            ui.add_space(2.0);
        });

        match self.tab {
            Tab::Import => {
                self.handle_import_keys(&ctx);
                if self.confirm_delete {
                    let n = self.marked.len();
                    let device = self.selected.as_ref().is_some_and(|s| s.device);
                    let body = if device {
                        format!("고른 {n}곡을 아이팟에서 지웁니다. 재생 목록에서도 빠집니다.\n파일은 아이팟 안 .podlift-old 폴더에 옮겨 두며, '아이팟에 넣기' 탭의 '마지막 작업 되돌리기'로 되돌릴 수 있습니다(그 사이 Finder 동기화를 하지 않았을 때).")
                    } else {
                        format!("고른 {n}개 파일을 휴지통으로 옮깁니다.")
                    };
                    match crate::popup::confirm_delete(&ctx, &p, "import", "곡을 삭제할까요?", &body, &format!("{n}곡 삭제")) {
                        Some(true) => {
                            self.confirm_delete = false;
                            self.start_delete();
                        }
                        Some(false) => self.confirm_delete = false,
                        None => {}
                    }
                }
            }
            Tab::Sync => {
                self.sync.show(ui, &p, &self.ipods);
                self.save_cfg();
                return;
            }
            Tab::Dedupe => {
                let sync_busy = self.sync.is_busy();
                self.dedupe.show(ui, &p, &mut self.cfg, &mut self.cfg_dirty, &self.ipods, sync_busy);
                self.save_cfg();
                return;
            }
            Tab::Organize => {
                self.organize.show(ui, &p, &mut self.cfg, &mut self.cfg_dirty);
                self.save_cfg();
                return;
            }
        }

        egui::Panel::bottom("actions").frame(pad).show(ui, |ui| {
            ui.add_space(6.0);
            if let Some((msg, is_err)) = self.notice.clone() {
                let c = if is_err { p.red } else { p.green };
                theme::banner(&p, c).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.add(egui::Label::new(RichText::new(msg).color(p.text)).wrap());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("닫기").clicked() {
                                self.notice = None;
                            }
                        });
                    });
                });
            }
            self.action_bar(ui, &p);
        });

        egui::CentralPanel::default().frame(pad).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 10.0;
            theme::card(ui, &p, |ui| self.ipod_card(ui, &p));
            theme::card(ui, &p, |ui| self.output_card(ui, &p));
            // 남는 높이는 전부 곡 목록에 준다
            theme::card(ui, &p, |ui| {
                ui.set_min_height(ui.available_height());
                self.library_card(ui, &p);
            });
        });

        self.save_cfg();
    }
}

impl PodliftApp {
    fn save_cfg(&mut self) {
        if self.cfg_dirty {
            if let Err(e) = self.cfg.save() {
                self.notice = Some((format!("설정 저장 실패: {e}"), true));
            }
            self.cfg_dirty = false;
        }
    }
}
