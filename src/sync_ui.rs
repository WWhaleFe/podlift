//! 아이팟에 넣기 탭 화면: 아이팟 고르기(곡 DB 상태) → 넣을 곡 모으기(파일·폴더·끌어다 놓기)
//! → 곡마다 판정(새 곡 / 더 좋은 음질로 바꿈 / 이미 있음 / 목록 안 중복 / 넣을 수 없음) → 넣기.
//! 실제 작업은 sync.rs, DB 형식은 itunesdb.rs.
use crate::app::human_bytes;
use crate::artsearch::{self, BatchMsg};
use crate::audio::{self, AudioInfo};
use crate::egui;
use crate::ipod::{self, Ipod};
use crate::ipod_art::{self, ArtSource, Donor, Need};
use crate::itunesdb::{self, DbTrack};
use crate::sync::{self, Decision, SyncEvent, SyncJob};
use crate::theme::{self, Palette};
use egui::{Color32, RichText};
use egui_extras::{Column, TableBuilder};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};

/// 고른 아이팟의 곡 DB 상태 (읽기 전용으로 연 결과)
struct DbState {
    tracks: Vec<DbTrack>,
    free: Option<u64>,
    /// 서명용 고유 번호 (서명이 필요 없는 DB 면 None 이어도 된다)
    firewire_id: Option<Vec<u8>>,
    needs_hash: bool,
    /// 되돌릴 수 있는 마지막 작업이 있는지
    can_restore: bool,
}

#[derive(Clone, PartialEq)]
enum RowStatus {
    Working,
    Done,
    Failed(String),
}

struct Row {
    info: AudioInfo,
    /// 넣을 앨범 아트가 있는지 (곡에 든 그림 또는 같은 폴더의 cover.jpg 등)
    has_cover: bool,
    decision: Decision,
    include: bool,
    /// 사용자가 '넣기' 체크를 직접 바꿨는지 (판정을 다시 해도 덮어쓰지 않는다)
    user_set: bool,
    status: Option<RowStatus>,
}

enum ReadEvent {
    Info(AudioInfo, bool),
    Done,
}

#[derive(Default)]
struct Progress {
    total: usize,
    done: usize,
    current: Option<usize>,
    backup: Option<PathBuf>,
}

pub struct SyncTab {
    mount: Option<PathBuf>,
    db: Option<Result<DbState, String>>,
    db_rx: Option<Receiver<Result<DbState, String>>>,
    rows: Vec<Row>,
    reader: Option<Receiver<ReadEvent>>,
    reading_left: usize,
    job: Option<SyncJob>,
    progress: Progress,
    /// 결과·안내 (문구, 오류 여부)
    notice: Option<(String, bool)>,
    confirm_restore: bool,
    /// 중복으로 판정된 곡을 직접 체크해 넣으려 할 때 한 번 더 묻는다
    confirm_dups: bool,
    eject_rx: Option<Receiver<Result<(), String>>>,
    /// '앨범 아트 채우기' 창
    art_open: bool,
    art_rx: Option<Receiver<(Vec<Need>, Vec<Donor>)>>,
    /// (앨범, 넣기 체크)
    art_rows: Option<Vec<(Need, bool)>>,
    art_job: Option<ipod_art::Job>,
    /// 온라인 검색 (찾은 수, 전체)
    art_search: Option<Receiver<BatchMsg>>,
    art_search_progress: (usize, usize),
    art_search_note: Option<String>,
    /// 그림을 빌려줄 수 있는 앨범, '다른 앨범의 그림 고르기' 창(줄, 검색어), 미리 보기
    art_donors: Vec<Donor>,
    art_picker: Option<(usize, String)>,
    art_textures: HashMap<PathBuf, Option<egui::TextureHandle>>,
}

impl SyncTab {
    pub fn new() -> Self {
        let mut tab = Self {
            mount: None,
            db: None,
            db_rx: None,
            rows: Vec::new(),
            reader: None,
            reading_left: 0,
            job: None,
            progress: Progress::default(),
            notice: None,
            confirm_restore: false,
            confirm_dups: false,
            eject_rx: None,
            art_open: false,
            art_rx: None,
            art_rows: None,
            art_job: None,
            art_search: None,
            art_search_progress: (0, 0),
            art_search_note: None,
            art_donors: Vec::new(),
            art_picker: None,
            art_textures: HashMap::new(),
        };
        // 개발·점검용: PODLIFT_SYNC_ADD=<폴더> 로 실행하면 그 폴더의 곡을 넣을 목록에 미리 담는다.
        if let Some(dir) = std::env::var_os("PODLIFT_SYNC_ADD") {
            tab.add_paths(vec![PathBuf::from(dir)]);
        }
        tab
    }

    pub fn is_busy(&self) -> bool {
        self.job.is_some() || self.reader.is_some() || self.db_rx.is_some() || self.eject_rx.is_some() || self.art_rx.is_some() || self.art_job.is_some() || self.art_search.is_some()
    }

    /// 다른 탭이 아이팟 곡 DB 를 고쳤을 때 다시 읽는다
    pub fn reload(&mut self) {
        if self.job.is_none() && self.db_rx.is_none() {
            self.db = None;
            self.load_db();
        }
    }

    fn select(&mut self, mount: Option<PathBuf>) {
        self.mount = mount;
        self.db = None;
        self.confirm_restore = false;
        self.load_db();
    }

    /// 곡 DB 를 백그라운드에서 읽는다 (수만 곡이면 1~2초 걸린다)
    fn load_db(&mut self) {
        let Some(mount) = self.mount.clone() else { return };
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let state = sync::open_db(&mount).map(|db| DbState {
                tracks: db.tracks(),
                free: ipod::free_space(&mount),
                needs_hash: db.hashing_scheme() == 1,
                firewire_id: ipod::firewire_id(&mount).and_then(|s| itunesdb::parse_firewire_id(&s)),
                can_restore: sync::last_backup(&mount).is_some(),
            });
            let _ = tx.send(state);
        });
        self.db_rx = Some(rx);
    }

    /// 파일·폴더를 넣을 목록에 더한다. 곡 정보는 백그라운드에서 읽는다(읽는 동안에는 더 받지 않는다).
    fn add_paths(&mut self, paths: Vec<PathBuf>) {
        if self.reader.is_some() {
            return;
        }
        let known: HashSet<PathBuf> = self.rows.iter().map(|r| r.info.path.clone()).collect();
        let mut files: Vec<PathBuf> = Vec::new();
        for p in paths {
            if p.is_dir() {
                for e in walkdir::WalkDir::new(&p).into_iter().flatten() {
                    if e.file_type().is_file() && is_input(e.path()) {
                        files.push(e.into_path());
                    }
                }
            } else if is_input(&p) {
                files.push(p);
            }
        }
        files.retain(|f| !known.contains(f));
        files.sort();
        files.dedup();
        if files.is_empty() {
            self.notice = Some(("넣을 수 있는 음악 파일이 없습니다.".into(), true));
            return;
        }
        self.reading_left = files.len();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for f in files {
                let has_cover = audio::cover_bytes(&f).is_some();
                let _ = tx.send(ReadEvent::Info(audio::read(&f, false), has_cover));
            }
            let _ = tx.send(ReadEvent::Done);
        });
        self.reader = Some(rx);
    }

    fn redecide(&mut self) {
        let Some(Ok(db)) = &self.db else {
            return;
        };
        let infos: Vec<AudioInfo> = self.rows.iter().map(|r| r.info.clone()).collect();
        let decisions = sync::decide(&infos, &db.tracks);
        for (row, d) in self.rows.iter_mut().zip(decisions) {
            if !row.user_set || matches!(d, Decision::Unsupported(_)) {
                row.include = d.default_include();
            }
            row.decision = d;
        }
    }

    pub fn pump(&mut self) {
        if let Some(rx) = &self.art_rx {
            if let Ok((needs, donors)) = rx.try_recv() {
                self.art_donors = donors;
                self.art_picker = None;
                self.art_rows = Some(needs.into_iter().map(|n| (n.source.is_some(), n)).map(|(on, n)| (n, on)).collect());
                self.art_rx = None;
            }
        }
        if let Some(rx) = &self.art_search {
            let mut done = false;
            for m in rx.try_iter() {
                match m {
                    BatchMsg::Progress(d, t) => self.art_search_progress = (d, t),
                    BatchMsg::Found(i, f) => {
                        if let Some((need, on)) = self.art_rows.as_mut().and_then(|r| r.get_mut(i)) {
                            need.source = Some(ArtSource::Online(f.file));
                            *on = true;
                        }
                    }
                    BatchMsg::Done { found, error } => {
                        let mut text = format!("앨범 {}개 중 {found}개의 그림을 온라인에서 찾았습니다.", self.art_search_progress.1);
                        if let Some(e) = error {
                            text.push_str(&format!(" 일부 검색은 실패했습니다: {e}"));
                        }
                        self.art_search_note = Some(text);
                        done = true;
                    }
                }
            }
            if done {
                self.art_search = None;
            }
        }
        if let Some(job) = &self.art_job {
            if let Some(result) = job.poll() {
                self.art_job = None;
                self.art_open = false;
                self.art_rows = None;
                self.notice = Some(match result {
                    Ok(n) => (format!("곡 {n}개에 앨범 아트를 채웠습니다. '마지막 작업 되돌리기'로 되돌릴 수 있습니다."), false),
                    Err(e) => (format!("앨범 아트를 채우지 못했습니다: {e}"), true),
                });
                self.load_db();
            }
        }
        if let Some(rx) = &self.db_rx {
            if let Ok(state) = rx.try_recv() {
                self.db = Some(state);
                self.db_rx = None;
                self.redecide();
            }
        }
        if let Some(rx) = &self.reader {
            let mut done = false;
            let mut added = false;
            for ev in rx.try_iter() {
                match ev {
                    ReadEvent::Info(info, has_cover) => {
                        self.reading_left = self.reading_left.saturating_sub(1);
                        self.rows.push(Row { info, has_cover, decision: Decision::New, include: true, user_set: false, status: None });
                        added = true;
                    }
                    ReadEvent::Done => done = true,
                }
            }
            if done {
                self.reader = None;
                self.reading_left = 0;
            }
            if added || done {
                self.redecide();
            }
        }
        if let Some(job) = &self.job {
            let mut finished = None;
            for ev in job.poll() {
                match ev {
                    SyncEvent::Backup(p) => self.progress.backup = Some(p),
                    SyncEvent::Begin(i) => {
                        self.progress.current = Some(i);
                        self.rows[i].status = Some(RowStatus::Working);
                    }
                    SyncEvent::Done(i, r) => {
                        self.progress.done += 1;
                        self.rows[i].status = Some(match r {
                            Ok(()) => RowStatus::Done,
                            Err(e) => RowStatus::Failed(e),
                        });
                    }
                    SyncEvent::Finished { added, replaced, art, error, note } => finished = Some((added, replaced, art, error, note)),
                }
            }
            if let Some((added, replaced, art, error, note)) = finished {
                self.job = None;
                self.progress.current = None;
                let failed = self.rows.iter().filter(|r| matches!(r.status, Some(RowStatus::Failed(_)))).count();
                let mut msg = format!("새로 넣음 {added}곡, 더 좋은 음질로 바꿈 {replaced}곡, 앨범 아트 {art}곡");
                if failed > 0 {
                    msg.push_str(&format!(", 실패 {failed}곡"));
                }
                if let Some(n) = &note {
                    msg.push_str(&format!(". {n}"));
                }
                let is_err = error.is_some() || failed > 0 || note.is_some();
                if let Some(e) = error {
                    msg = format!("{e}  ({msg})");
                } else {
                    msg.push_str(". 아이팟을 꺼낸 뒤 확인하세요.");
                }
                self.notice = Some((msg, is_err));
                // 넣은 곡은 목록에서 빼고, DB 를 다시 읽어 남은 곡의 판정을 고친다
                self.rows.retain(|r| r.status != Some(RowStatus::Done));
                self.load_db();
            }
        }
        if let Some(rx) = &self.eject_rx {
            if let Ok(r) = rx.try_recv() {
                self.eject_rx = None;
                self.notice = Some(match r {
                    Ok(()) => ("아이팟을 꺼냈습니다. 이제 케이블을 뽑아도 됩니다.".into(), false),
                    Err(e) => (format!("꺼내지 못했습니다: {e}"), true),
                });
            }
        }
    }

    fn start(&mut self) {
        let (Some(mount), Some(Ok(db))) = (self.mount.clone(), &self.db) else { return };
        let items: Vec<sync::Item> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.include && !matches!(r.decision, Decision::Unsupported(_)))
            .map(|(i, r)| sync::Item {
                index: i,
                info: r.info.clone(),
                replace: match r.decision {
                    Decision::Better(id) => Some(id),
                    _ => None,
                },
            })
            .collect();
        if items.is_empty() {
            return;
        }
        let need: u64 = items.iter().map(|i| i.info.size).sum();
        if let Some(free) = db.free {
            // 변환하면 크기가 달라지고 DB 도 커지므로 10MB 여유를 두고 본다
            if need + 10_000_000 > free {
                self.notice = Some((format!("아이팟 남은 공간이 부족합니다. 넣을 곡 {} + 여유 10 MB 가 필요한데 {} 남았습니다.", human_bytes(need), human_bytes(free)), true));
                return;
            }
        }
        if db.needs_hash && db.firewire_id.is_none() {
            self.notice = Some(("이 아이팟의 고유 번호(FirewireGuid)를 찾지 못했습니다. 케이블을 다시 꽂고 '다시 읽기'를 눌러 보세요.".into(), true));
            return;
        }
        for r in &mut self.rows {
            r.status = None;
        }
        self.progress = Progress { total: items.len(), ..Default::default() };
        self.notice = None;
        self.confirm_restore = false;
        self.job = Some(sync::start(mount, items, db.firewire_id.clone()));
    }

    fn restore(&mut self) {
        let Some(mount) = self.mount.clone() else { return };
        self.confirm_restore = false;
        self.notice = Some(match sync::restore_last(&mount) {
            Ok(n) => (format!("마지막 작업 전으로 되돌렸습니다. 그때 넣은 파일 {n}개를 지웠습니다."), false),
            Err(e) => (format!("되돌리지 못했습니다: {e}"), true),
        });
        self.load_db();
    }

    // ── 화면 ──────────────────────────────────────────

    pub fn show(&mut self, ui: &mut egui::Ui, p: &Palette, ipods: &[Ipod]) {
        // 연결이 끊긴 아이팟은 선택에서 뺀다. 고른 것이 없으면 첫 번째를 고른다.
        let devices: Vec<&Ipod> = ipods.iter().filter(|i| i.device).collect();
        if self.job.is_none() {
            if let Some(m) = &self.mount {
                if !devices.iter().any(|i| &i.mount == m) {
                    self.select(None);
                }
            }
            if self.mount.is_none() {
                if let Some(first) = devices.first() {
                    self.select(Some(first.mount.clone()));
                }
            }
        }
        // Finder 에서 끌어다 놓은 파일·폴더
        let dropped: Vec<PathBuf> = ui.ctx().input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect());
        if !dropped.is_empty() && self.job.is_none() && self.reader.is_none() {
            self.add_paths(dropped);
        }

        let pad = egui::Frame::new().inner_margin(egui::Margin { left: 18, right: 18, top: 0, bottom: 10 });
        egui::Panel::bottom("sync-actions").frame(pad).show(ui, |ui| {
            ui.add_space(6.0);
            self.action_bar(ui, p);
        });
        if self.art_open {
            self.art_window(ui.ctx(), p);
        }
        egui::CentralPanel::default().frame(pad).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 10.0;
            theme::card(ui, p, |ui| self.ipod_card(ui, p, &devices));
            theme::card(ui, p, |ui| {
                ui.set_min_height(ui.available_height());
                self.queue_card(ui, p);
            });
        });
    }

    /// 아트가 없는 곡을 백그라운드에서 찾아 창을 연다 (곡 파일을 읽어 그림이 들었는지 본다)
    fn open_art(&mut self) {
        let (Some(mount), Some(Ok(db))) = (self.mount.clone(), &self.db) else { return };
        let tracks = db.tracks.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let needs = ipod_art::needs(&mount, &tracks);
            let donors = ipod_art::donors(&tracks, &needs);
            let _ = tx.send((needs, donors));
        });
        self.art_rx = Some(rx);
        self.art_rows = None;
        self.art_open = true;
    }

    fn art_window(&mut self, ctx: &egui::Context, p: &Palette) {
        let mut start = false;
        let mut open_picker: Option<(usize, String)> = None;
        let open = crate::popup::native_window(ctx, "ipod-art", "앨범 아트 채우기", [760.0, 560.0], p, |ui| {
            if self.art_job.is_some() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("그림을 저장하고 곡 DB 를 고치는 중입니다… 아이팟을 빼지 마세요.").color(p.text));
                });
                return;
            }
            let Some(rows) = &mut self.art_rows else {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("아트가 없는 곡과 곡 파일 속 그림을 찾는 중입니다…").color(p.muted));
                });
                return;
            };
            ui.label(
                RichText::new("같은 앨범의 다른 곡이 아이팟에서 쓰는 그림이 있으면 그것을 같이 쓰고, 없으면 곡 파일에 든 그림을 씁니다. 둘 다 없는 앨범은 그림 파일을 직접 고르세요.")
                    .size(13.0)
                    .color(p.muted),
            );
            let songs: usize = rows.iter().filter(|(n, on)| *on && n.source.is_some()).map(|(n, _)| n.track_ids.len()).sum();
            let missing: Vec<(usize, String, String)> =
                rows.iter().enumerate().filter(|(_, (n, _))| n.source.is_none() && !n.album.trim().is_empty()).map(|(i, (n, _))| (i, n.album.clone(), n.artist.clone())).collect();
            let searching = self.art_search.is_some();
            ui.horizontal(|ui| {
                theme::chip(ui, &format!("아트 없는 앨범 {}", rows.len()), p.accent, true);
                theme::chip(ui, &format!("채울 곡 {songs}"), p.blue, false);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::primary_button(ui, p, &format!("곡 {songs}개에 채우기"), songs > 0 && !searching).clicked() {
                        start = true;
                    }
                    if searching {
                        let (d, t) = self.art_search_progress;
                        ui.spinner();
                        ui.label(RichText::new(format!("온라인 검색 {d} / {t}")).color(p.muted));
                    } else if !missing.is_empty()
                        && theme::tinted_button(ui, p.accent, &format!("그림 없는 {}개 온라인에서 찾기", missing.len()), egui::vec2(220.0, 32.0))
                            .on_hover_text("MusicBrainz(Cover Art Archive)와 iTunes 에서 앨범 이름과 가수가 맞는 표지를 찾습니다. 보내는 것은 앨범·가수 이름뿐입니다.")
                            .clicked()
                    {
                        self.art_search_progress = (0, missing.len());
                        self.art_search_note = None;
                        self.art_search = Some(artsearch::start_batch(missing.clone()));
                    }
                });
            });
            if let Some(note) = &self.art_search_note {
                ui.label(RichText::new(note).size(12.5).color(p.green));
            }
            let has_donors = !self.art_donors.is_empty();
            egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, 46.0, rows.len(), |ui, range| {
                let start = range.start;
                for (k, (need, on)) in rows[range].iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        ui.set_height(42.0);
                        ui.add_enabled(need.source.is_some(), egui::Checkbox::without_text(on));
                        let text_w = (ui.available_width() - 150.0).max(200.0);
                        ui.vertical(|ui| {
                            ui.set_max_width(text_w);
                            let album = if need.album.is_empty() { "(앨범 없음)".to_string() } else { need.album.clone() };
                            ui.add(egui::Label::new(RichText::new(format!("{album}  ·  {}", need.artist)).font(theme::bold(13.5)).color(p.text)).truncate());
                            let (src, color) = match (&need.source, &need.borrowed) {
                                (Some(_), Some(from)) => (format!("고른 앨범의 그림  ·  {from}"), p.muted),
                                (Some(s), None) => (s.label().to_string(), p.muted),
                                (None, _) => ("그림 없음 - 다른 앨범에서 고르거나 그림 파일을 고르세요".to_string(), p.amber),
                            };
                            ui.label(RichText::new(format!("곡 {}개  ·  {src}", need.track_ids.len())).size(12.5).color(color));
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("그림 고르기…").clicked() {
                                if let Some(f) = rfd::FileDialog::new().set_title("앨범 아트 그림 선택").add_filter("그림", &["jpg", "jpeg", "png"]).pick_file() {
                                    need.source = Some(ArtSource::Picked(f));
                                    need.borrowed = None;
                                    *on = true;
                                }
                            }
                            if has_donors && ui.button("다른 앨범에서…").on_hover_text("그림이 있는 다른 앨범을 이름이 비슷한 순서로 보여 줍니다. 고른 앨범의 그림을 씁니다.").clicked() {
                                open_picker = Some((start + k, need.album.clone()));
                            }
                        });
                    });
                }
            });
        });
        if let Some(pk) = open_picker {
            self.art_picker = Some(pk);
        }
        if self.art_picker.is_some() {
            self.art_picker_window(ctx, p);
        }
        if start {
            if let (Some(mount), Some(rows), Some(Ok(db))) = (self.mount.clone(), &self.art_rows, &self.db) {
                let work: Vec<(Vec<u32>, ArtSource)> = rows.iter().filter(|(_, on)| *on).filter_map(|(n, _)| Some((n.track_ids.clone(), n.source.clone()?))).collect();
                self.art_job = Some(ipod_art::start(mount, work, db.firewire_id.clone()));
            }
        }
        if !open && self.art_job.is_none() {
            self.art_open = false;
            self.art_rows = None;
            self.art_search = None;
            self.art_search_note = None;
            self.art_picker = None;
            self.art_textures.clear();
        }
    }

    /// 그림이 있는 다른 앨범 목록 창 (아이팟). 고르면 그 앨범의 그림을 이 앨범 곡에 쓴다.
    fn art_picker_window(&mut self, ctx: &egui::Context, p: &Palette) {
        let Some((row, query)) = self.art_picker.as_mut() else { return };
        let row = *row;
        let Some(target) = self.art_rows.as_ref().and_then(|r| r.get(row)).map(|(n, _)| (n.album.clone(), n.artist.clone())) else {
            self.art_picker = None;
            return;
        };
        let mut chosen: Option<usize> = None;
        let title = format!("다른 앨범의 그림 고르기 - {}", if target.0.is_empty() { "(앨범 없음)" } else { &target.0 });
        let open = crate::popup::native_window(ctx, "ipod-art-picker", &title, [620.0, 560.0], p, |ui| {
            ui.horizontal(|ui| {
                ui.label("찾기");
                ui.add(egui::TextEdit::singleline(query).desired_width(ui.available_width()).hint_text("앨범이나 가수 이름"));
            });
            ui.label(RichText::new("이름이 비슷한 앨범부터 보여 줍니다. 고른 앨범의 그림을 이 앨범의 아트 없는 곡에 붙입니다.").size(12.5).color(p.muted));
            let mut order: Vec<(f32, usize)> = self
                .art_donors
                .iter()
                .enumerate()
                .map(|(i, d)| {
                    let score = if query.trim() == target.0.trim() {
                        artsearch::similarity(&target.0, &target.1, &d.album, &d.artist)
                    } else {
                        artsearch::query_score(query, &d.album, &d.artist)
                    };
                    (score, i)
                })
                .collect();
            order.sort_by(|a, b| b.0.total_cmp(&a.0));
            order.truncate(200);
            let Self { art_donors, art_textures, .. } = self;
            egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, 64.0, order.len(), |ui, range| {
                for &(_, i) in &order[range] {
                    let d = &art_donors[i];
                    ui.horizontal(|ui| {
                        ui.set_height(60.0);
                        let (key, empty) = match &d.source {
                            ArtSource::Existing(id) => (PathBuf::from(format!("ipod-image-{id}")), "아이팟 그림"),
                            ArtSource::Embedded(f) | ArtSource::Picked(f) | ArtSource::Online(f) => (f.clone(), "없음"),
                        };
                        crate::organize_ui::cover_thumb(ui, p, ctx, art_textures, &key, empty, || d.source.bytes());
                        ui.vertical(|ui| {
                            ui.set_max_width((ui.available_width() - 120.0).max(160.0));
                            ui.add(egui::Label::new(RichText::new(&d.album).font(theme::bold(13.5)).color(p.text)).truncate());
                            ui.add(egui::Label::new(RichText::new(&d.artist).size(12.5).color(p.muted)).truncate());
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("이 그림 쓰기").clicked() {
                                chosen = Some(i);
                            }
                        });
                    });
                }
            });
        });
        if let Some(i) = chosen {
            let d = self.art_donors[i].clone();
            if let Some((need, on)) = self.art_rows.as_mut().and_then(|rows| rows.get_mut(row)) {
                need.source = Some(d.source);
                need.borrowed = Some(d.album);
                *on = true;
            }
            self.art_picker = None;
        } else if !open {
            self.art_picker = None;
        }
    }

    fn ipod_card(&mut self, ui: &mut egui::Ui, p: &Palette, devices: &[&Ipod]) {
        ui.label(RichText::new("넣을 아이팟").font(theme::bold(16.0)).color(p.text));
        if devices.is_empty() {
            ui.label(RichText::new("연결된 아이팟이 없습니다. 케이블로 연결하면 저절로 나타납니다.").size(13.5).color(p.muted));
            return;
        }
        let mut pick = None;
        ui.horizontal_wrapped(|ui| {
            for d in devices {
                let on = self.mount.as_ref() == Some(&d.mount);
                let label = if d.model.is_empty() { d.name.clone() } else { format!("{}  ·  {}", d.name, d.model) };
                let text = if on { RichText::new(label).font(theme::bold(14.0)).color(p.accent_text) } else { RichText::new(label).color(p.text) };
                let btn = egui::Button::new(text)
                    .fill(if on { p.accent } else { p.card_strong })
                    .stroke(egui::Stroke::new(1.0, if on { p.accent } else { p.border }))
                    .corner_radius(10);
                if ui.add_enabled(self.job.is_none() || on, btn).clicked() && !on {
                    pick = Some(d.mount.clone());
                }
            }
        });
        if let Some(m) = pick {
            self.select(Some(m));
        }
        match &self.db {
            None => {
                ui.label(RichText::new("곡 DB 읽는 중…").size(13.5).color(p.muted));
            }
            Some(Err(e)) => {
                theme::banner(p, p.red).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new(format!("이 아이팟에는 넣을 수 없습니다: {e}")).color(p.text));
                });
            }
            Some(Ok(db)) => {
                let mut restore = false;
                let mut reload = false;
                let mut open_art = false;
                ui.horizontal_wrapped(|ui| {
                    theme::chip(ui, &format!("아이팟 곡 {}", db.tracks.len()), p.accent, true);
                    if let Some(free) = db.free {
                        theme::chip(ui, &format!("남은 공간 {}", human_bytes(free)), p.blue, false);
                    }
                    if db.needs_hash {
                        let (t, c) = if db.firewire_id.is_some() { ("서명 준비됨", p.green) } else { ("고유 번호 없음", p.red) };
                        theme::chip(ui, t, c, false).on_hover_text("아이팟 클래식은 곡 DB 에 기기 고유 번호로 만든 서명(hash58)이 있어야 곡 목록을 읽습니다.");
                    }
                    let no_art = db.tracks.iter().filter(|t| !t.has_artwork || t.mhii_link == 0).count();
                    theme::chip(ui, &format!("아트 없는 곡 {no_art}"), p.amber, false);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_enabled_ui(self.job.is_none() && self.art_job.is_none(), |ui| {
                            if no_art > 0 && ui.button("앨범 아트 채우기…").on_hover_text("아이팟에 있지만 앨범 아트가 없는 곡에 그림을 붙입니다.").clicked() {
                                open_art = true;
                            }
                            if ui.button("다시 읽기").clicked() {
                                reload = true;
                            }
                            if db.can_restore {
                                if self.confirm_restore {
                                    if theme::tinted_button(ui, p.red, "되돌리기", egui::vec2(96.0, 30.0)).clicked() {
                                        restore = true;
                                    }
                                    if ui.button("취소").clicked() {
                                        self.confirm_restore = false;
                                    }
                                    ui.label(RichText::new("마지막으로 넣기 전 상태로 되돌릴까요?").color(p.amber));
                                } else if ui.button("마지막 작업 되돌리기").on_hover_text("곡 DB 를 마지막 작업 전 백업으로 되돌립니다(넣기·중복 정리·앨범 아트 채우기). 그 뒤에 Finder 동기화 등이 DB 를 바꿨으면 되돌리지 않습니다.").clicked() {
                                    self.confirm_restore = true;
                                }
                            }
                        });
                    });
                });
                if restore {
                    self.restore();
                }
                if reload {
                    self.load_db();
                }
                if open_art {
                    self.open_art();
                }
            }
        }
    }

    fn queue_card(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let count = |f: &dyn Fn(&Decision) -> bool| self.rows.iter().filter(|r| f(&r.decision)).count();
        let new = count(&|d| *d == Decision::New);
        let better = count(&|d| matches!(d, Decision::Better(_)));
        let exists = count(&|d| matches!(d, Decision::Exists(_)));
        let dup = count(&|d| matches!(d, Decision::QueueDup(_)));
        let bad = count(&|d| matches!(d, Decision::Unsupported(_)));
        let convert = self.rows.iter().filter(|r| matches!(sync::conversion(&r.info), Ok(Some(_)))).count();
        let covers = self.rows.iter().filter(|r| r.has_cover).count();
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("넣을 곡").font(theme::bold(16.0)).color(p.text));
            if !self.rows.is_empty() {
                if new > 0 {
                    theme::chip(ui, &format!("새 곡 {new}"), p.green, true);
                }
                if better > 0 {
                    theme::chip(ui, &format!("더 좋은 음질로 바꿈 {better}"), p.blue, true)
                        .on_hover_text("아이팟에 있는 같은 곡보다 음질이 분명히 좋습니다. 재생 횟수·별점·재생 목록은 그대로 두고 파일만 바꿉니다.");
                }
                if exists + dup > 0 {
                    theme::chip(ui, &format!("중복 감지 {}", exists + dup), p.amber, true).on_hover_text(
                        "같은 곡의 기준: 앨범+제목이 같거나(가수가 다르면 길이도 같아야 함), 앨범이 달라도 제목+가수가 같은 곡. '겹치는 곡' 칸에 어느 곡과 겹치는지 나옵니다.",
                    );
                }
                if exists > 0 {
                    theme::chip(ui, &format!("아이팟에 이미 있음 {exists}"), p.teal, false).on_hover_text("아이팟에 같은 곡이 같거나 더 좋은 음질로 있어 건너뜁니다.");
                }
                if dup > 0 {
                    theme::chip(ui, &format!("목록 안 중복 {dup}"), p.muted, false).on_hover_text("넣을 목록 안에 더 좋은 음질의 같은 곡이 있어 건너뜁니다.");
                }
                if bad > 0 {
                    theme::chip(ui, &format!("넣을 수 없음 {bad}"), p.red, false);
                }
                if covers > 0 {
                    theme::chip(ui, &format!("앨범 아트 {covers}"), p.teal, false)
                        .on_hover_text("곡에 든 그림이나 같은 폴더의 cover.jpg·folder.jpg 를 아이팟 화면용으로 넣습니다. 같은 앨범은 그림 한 장을 같이 쓰고, 아이팟에 이미 그 앨범 그림이 있으면 그것을 씁니다.");
                }
                if convert > 0 {
                    theme::chip(ui, &format!("변환해서 넣음 {convert}"), p.amber, false)
                        .on_hover_text("FLAC·OGG 같은 형식이나 48kHz·16bit 를 넘는 고해상도 파일은 아이팟 클래식이 재생할 수 있게 ALAC(무손실) 또는 AAC 256kbps 로 바꿔 넣습니다. 원본 파일은 그대로 둡니다.");
                }
            }
        });
        ui.horizontal(|ui| {
            ui.add_enabled_ui(self.job.is_none() && self.reader.is_none(), |ui| {
                if ui.button("파일 추가…").clicked() {
                    if let Some(files) = rfd::FileDialog::new()
                        .set_title("아이팟에 넣을 음악 파일")
                        .add_filter("음악", sync::INPUT_EXT)
                        .pick_files()
                    {
                        self.add_paths(files);
                    }
                }
                if ui.button("폴더 추가…").clicked() {
                    if let Some(dirs) = rfd::FileDialog::new().set_title("아이팟에 넣을 음악 폴더").pick_folders() {
                        self.add_paths(dirs);
                    }
                }
                if !self.rows.is_empty() && ui.button("목록 비우기").clicked() {
                    self.rows.clear();
                }
            });
            if self.reader.is_some() {
                ui.spinner();
                ui.label(RichText::new(format!("곡 정보 읽는 중… {}개 남음", self.reading_left)).color(p.muted));
            }
        });
        if self.rows.is_empty() {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("Finder 에서 음악 파일이나 폴더를 이 창으로 끌어다 놓으세요.").size(15.0).color(p.muted));
                ui.label(RichText::new("앨범과 제목이 같은 곡이 아이팟에 이미 있으면 음질을 비교해 더 좋을 때만 바꿔 넣습니다.").size(13.0).color(p.muted));
                ui.label(RichText::new("앨범 아트도 함께 넣습니다. 넣기 전에 아이팟 곡 DB 와 그림 DB 를 자동으로 백업하며, 마지막 작업은 되돌릴 수 있습니다.").size(13.0).color(p.muted));
            });
            return;
        }
        self.table(ui, p);
    }

    fn table(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let follow = self.progress.current;
        let busy = self.job.is_some();
        let mut table = TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(40.0))
            .column(Column::initial(110.0).at_least(90.0).clip(true))
            .column(Column::initial(200.0).at_least(80.0).clip(true))
            .column(Column::exact(36.0))
            .column(Column::initial(220.0).at_least(100.0).clip(true))
            .column(Column::initial(150.0).at_least(80.0).clip(true))
            .column(Column::initial(160.0).at_least(80.0).clip(true))
            .column(Column::remainder().at_least(120.0).clip(true))
            .min_scrolled_height(0.0)
            .max_scroll_height(f32::INFINITY);
        if let Some(row) = follow {
            table = table.scroll_to_row(row, Some(egui::Align::Center));
        }
        let muted = p.muted;
        // 겹치는 아이팟 곡을 보여 주려고 번호로 찾는 표
        let ipod: std::collections::HashMap<u32, &DbTrack> = match &self.db {
            Some(Ok(db)) => db.tracks.iter().map(|t| (t.id, t)).collect(),
            _ => std::collections::HashMap::new(),
        };
        let queue_titles: Vec<(String, String)> = self.rows.iter().map(|r| (r.info.title.clone(), r.info.quality_label())).collect();
        table
            .header(22.0, |mut h| {
                for t in ["넣기", "판정", "겹치는 곡", "아트", "제목", "가수", "앨범", "음질"] {
                    h.col(|ui| {
                        ui.label(RichText::new(t).font(theme::bold(13.0)).color(muted));
                    });
                }
            })
            .body(|body| {
                body.rows(26.0, self.rows.len(), |mut row| {
                    let r = &mut self.rows[row.index()];
                    let unsupported = matches!(r.decision, Decision::Unsupported(_));
                    row.col(|ui| {
                        ui.add_enabled_ui(!busy && !unsupported, |ui| {
                            if ui.checkbox(&mut r.include, "").changed() {
                                r.user_set = true;
                            }
                        });
                    });
                    row.col(|ui| {
                        let (text, color, tip) = status_label(r, p);
                        let resp = ui.label(RichText::new(text).size(13.0).color(color));
                        if !tip.is_empty() {
                            resp.on_hover_text(tip);
                        }
                    });
                    row.col(|ui| {
                        let other = match r.decision {
                            Decision::Exists(id) | Decision::Better(id) => ipod.get(&id).map(|t| {
                                (format!("아이팟: {} - {}", t.title, t.artist), format!("아이팟의 곡\n{} - {}\n앨범: {}\n음질: {}", t.title, t.artist, t.album, db_quality(t)))
                            }),
                            Decision::QueueDup(j) => queue_titles.get(j).map(|(title, q)| (format!("목록 {}번: {title}", j + 1), format!("넣을 목록 {}번째 곡이 더 좋은 같은 곡입니다\n음질: {q}", j + 1))),
                            _ => None,
                        };
                        if let Some((text, tip)) = other {
                            ui.add(egui::Label::new(RichText::new(text).size(12.5).color(muted)).truncate()).on_hover_text(tip);
                        }
                    });
                    row.col(|ui| {
                        let (t, c) = if r.has_cover { ("있음", p.teal) } else { ("-", p.muted) };
                        ui.label(RichText::new(t).size(12.5).color(c));
                    });
                    let dim = if r.include { p.text } else { p.muted };
                    row.col(|ui| {
                        ui.add(egui::Label::new(RichText::new(&r.info.title).color(dim)).truncate()).on_hover_text(r.info.path.display().to_string());
                    });
                    row.col(|ui| {
                        ui.add(egui::Label::new(RichText::new(&r.info.artist).color(dim)).truncate());
                    });
                    row.col(|ui| {
                        ui.add(egui::Label::new(RichText::new(&r.info.album).color(dim)).truncate());
                    });
                    row.col(|ui| {
                        let mut q = r.info.quality_label();
                        if let Ok(Some(c)) = sync::conversion(&r.info) {
                            q.push_str(match c {
                                sync::Convert::Alac(_) => "  → ALAC",
                                sync::Convert::Aac => "  → AAC",
                            });
                        }
                        ui.add(egui::Label::new(RichText::new(q).size(12.5).color(muted)).truncate());
                    });
                });
            });
    }

    fn action_bar(&mut self, ui: &mut egui::Ui, p: &Palette) {
        if let Some((msg, is_err)) = self.notice.clone() {
            let c = if is_err { p.amber } else { p.green };
            theme::banner(p, c).show(ui, |ui| {
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
        if self.job.is_some() {
            let pr = &self.progress;
            let frac = if pr.total == 0 { 0.0 } else { pr.done as f32 / pr.total as f32 };
            ui.add(egui::ProgressBar::new(frac).text(format!("{} / {}곡 넣는 중", pr.done, pr.total)));
            ui.horizontal(|ui| {
                if let Some(r) = pr.current.and_then(|i| self.rows.get(i)) {
                    ui.add(egui::Label::new(RichText::new(format!("지금: {} - {}", r.info.artist, r.info.title)).color(p.muted)).truncate());
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::tinted_button(ui, p.red, "중지", egui::vec2(110.0, 36.0)).on_hover_text("지금 곡까지 마치고, 그때까지 넣은 곡은 아이팟에 등록합니다.").clicked() {
                        if let Some(j) = &self.job {
                            j.cancel();
                        }
                    }
                });
            });
            return;
        }
        let n = self.rows.iter().filter(|r| r.include && !matches!(r.decision, Decision::Unsupported(_))).count();
        let dups = self.rows.iter().filter(|r| r.include && matches!(r.decision, Decision::Exists(_) | Decision::QueueDup(_))).count();
        let ready = n > 0 && matches!(self.db, Some(Ok(_))) && self.reader.is_none() && self.eject_rx.is_none();
        if self.confirm_dups && (dups == 0 || !ready) {
            self.confirm_dups = false;
        }
        if self.confirm_dups {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("중복으로 감지된 곡 {dups}개가 함께 들어갑니다. 그래도 넣을까요?")).font(theme::bold(15.0)).color(p.amber));
                if theme::tinted_button(ui, p.amber, "중복 빼고 넣기", egui::vec2(140.0, 36.0)).clicked() {
                    for r in &mut self.rows {
                        if matches!(r.decision, Decision::Exists(_) | Decision::QueueDup(_)) {
                            r.include = false;
                        }
                    }
                    self.confirm_dups = false;
                    self.start();
                }
                if theme::ghost_button(ui, p, "그래도 넣기").clicked() {
                    self.confirm_dups = false;
                    self.start();
                }
                if theme::ghost_button(ui, p, "취소").clicked() {
                    self.confirm_dups = false;
                }
            });
            return;
        }
        ui.horizontal(|ui| {
            if theme::primary_button(ui, p, &format!("{n}곡 아이팟에 넣기"), ready).clicked() {
                if dups > 0 {
                    self.confirm_dups = true;
                } else {
                    self.start();
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(mount) = self.mount.clone() {
                    let ejecting = self.eject_rx.is_some();
                    ui.add_enabled_ui(!ejecting && self.reader.is_none(), |ui| {
                        if theme::ghost_button(ui, p, if ejecting { "꺼내는 중…" } else { "아이팟 꺼내기" }).clicked() {
                            let (tx, rx) = mpsc::channel();
                            std::thread::spawn(move || {
                                let _ = tx.send(ipod::eject(&mount));
                            });
                            self.eject_rx = Some(rx);
                        }
                    });
                }
            });
        });
    }
}

/// 판정 칸의 글자·색·설명 (작업 결과가 있으면 그것을 먼저)
fn status_label(r: &Row, p: &Palette) -> (String, Color32, String) {
    match &r.status {
        Some(RowStatus::Working) => return ("넣는 중".into(), p.blue, String::new()),
        Some(RowStatus::Done) => return ("넣음".into(), p.green, String::new()),
        Some(RowStatus::Failed(e)) => return ("실패".into(), p.red, e.clone()),
        None => {}
    }
    match &r.decision {
        Decision::New => ("새 곡".into(), p.green, String::new()),
        Decision::Better(_) => ("음질 개선".into(), p.blue, "아이팟의 같은 곡보다 음질이 좋아 파일을 바꿉니다".into()),
        Decision::Exists(_) => ("이미 있음".into(), p.teal, "아이팟에 같은 곡이 같거나 더 좋은 음질로 있습니다. 체크하면 따로 하나 더 넣습니다(중복)".into()),
        Decision::QueueDup(_) => ("목록 중복".into(), p.muted, "넣을 목록 안에 더 좋은 같은 곡이 있습니다. 체크하면 둘 다 들어갑니다(중복)".into()),
        Decision::Unsupported(e) => ("불가".into(), p.red, e.clone()),
    }
}

/// 아이팟 곡의 음질 한 줄
fn db_quality(t: &DbTrack) -> String {
    let khz = if t.sample_rate == 0 { String::new() } else { format!(" · {:.1}kHz", t.sample_rate as f64 / 1000.0) };
    if t.lossless {
        format!("무손실{khz}")
    } else {
        format!("{}kbps{khz}", t.bitrate)
    }
}

fn is_input(p: &Path) -> bool {
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    !name.starts_with('.')
        && p.extension().is_some_and(|e| sync::INPUT_EXT.contains(&e.to_string_lossy().to_ascii_lowercase().as_str()))
}
