//! 폴더 정리 탭 화면: 맥 음악 폴더를 '가수/앨범/트랙 제목' 구조로 정리하고,
//! 앨범 아트가 없는 곡 파일에 그림을 심는다. 작업은 organize.rs.
use crate::app::open_path;
use crate::artsearch::{self, BatchMsg};
use crate::audio::{self, AudioInfo};
use crate::config::Config;
use crate::egui;
use crate::organize::{self, ArtAlbum, ArtSource, Donor, Plan};
use crate::theme::{self, Palette};
use egui::RichText;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Structure,
    Art,
}

enum ScanMsg {
    Progress(usize, usize),
    Structure(Plan),
    Art(Vec<ArtAlbum>, Vec<Donor>),
    Failed(String),
}

enum JobMsg {
    Progress(usize, usize),
    Done(String, bool),
}

struct ArtRow {
    a: ArtAlbum,
    /// 선택 (아트 넣기·삭제 모두 고른 줄에만 한다)
    include: bool,
}

pub struct OrganizeTab {
    mode: Mode,
    scan: Option<Receiver<ScanMsg>>,
    cancel: Arc<AtomicBool>,
    progress: (usize, usize),
    /// 결과를 만든 폴더
    root: PathBuf,
    plan: Option<Plan>,
    arts: Option<Vec<ArtRow>>,
    job: Option<Receiver<JobMsg>>,
    job_progress: (usize, usize),
    confirm: bool,
    confirm_undo: bool,
    notice: Option<(String, bool)>,
    /// 앨범 아트 미리 보기 (그림 출처 경로 → 텍스처, 읽지 못하면 None)
    textures: HashMap<PathBuf, Option<egui::TextureHandle>>,
    /// 온라인 검색
    search: Option<Receiver<BatchMsg>>,
    search_progress: (usize, usize),
    /// 그림을 빌려줄 수 있는 앨범 (곡 파일에 그림이 든 앨범)
    donors: Vec<Donor>,
    /// '다른 앨범의 그림 고르기' 창: 어느 줄, 검색어
    picker: Option<(usize, String)>,
    /// 앨범 아트 목록의 키보드 커서와, 그 줄이 보이게 굴릴지
    art_cursor: Option<usize>,
    art_scroll: bool,
    /// 고른 곡 삭제 경고창을 띄웠는지
    confirm_art_delete: bool,
}

impl OrganizeTab {
    pub fn new() -> Self {
        let mut tab = Self {
            mode: Mode::Structure,
            scan: None,
            cancel: Arc::new(AtomicBool::new(false)),
            progress: (0, 0),
            root: PathBuf::new(),
            plan: None,
            arts: None,
            job: None,
            job_progress: (0, 0),
            confirm: false,
            confirm_undo: false,
            notice: None,
            textures: HashMap::new(),
            search: None,
            search_progress: (0, 0),
            donors: Vec::new(),
            picker: None,
            art_cursor: None,
            art_scroll: false,
            confirm_art_delete: false,
        };
        // 개발·점검용: PODLIFT_ORGANIZE_DIR=<폴더> 면 그 폴더를 바로 훑는다 (PODLIFT_ORGANIZE_MODE=art 면 앨범 아트 넣기)
        if let Ok(dir) = std::env::var("PODLIFT_ORGANIZE_DIR") {
            if std::env::var("PODLIFT_ORGANIZE_MODE").as_deref() == Ok("art") {
                tab.mode = Mode::Art;
            }
            tab.start_scan(Path::new(dir.trim()));
        }
        tab
    }

    /// 앨범 아트 목록 키보드: ↑↓ 이동, 스페이스 선택, ⌘A 전체 선택, Esc 전체 해제, Delete 고른 곡 삭제
    fn handle_art_keys(&mut self, ctx: &egui::Context) {
        if self.mode != Mode::Art || self.is_busy() || self.confirm || self.confirm_art_delete || self.picker.is_some() {
            return;
        }
        let Some(rows) = self.arts.as_mut() else { return };
        if rows.is_empty() {
            return;
        }
        let keys = crate::listkeys::take(ctx, false);
        if !keys.any() {
            return;
        }
        if keys.up || keys.down {
            self.art_cursor = crate::listkeys::step(self.art_cursor, rows.len(), &keys);
            self.art_scroll = true;
        }
        if keys.space {
            let c = self.art_cursor.unwrap_or(0).min(rows.len() - 1);
            self.art_cursor = Some(c);
            rows[c].include = !rows[c].include;
        }
        if keys.select_all {
            rows.iter_mut().for_each(|r| r.include = true);
        }
        if keys.clear {
            rows.iter_mut().for_each(|r| r.include = false);
        }
        if keys.delete && rows.iter().any(|r| r.include) {
            self.confirm_art_delete = true;
        }
    }

    /// 고른 앨범 줄의 (아트 없는) 곡 파일
    fn art_delete_files(&self) -> Vec<PathBuf> {
        self.arts.iter().flatten().filter(|r| r.include).flat_map(|r| r.a.missing.iter().cloned()).collect()
    }

    /// 고른 곡을 휴지통으로 옮긴다
    fn start_art_delete(&mut self) {
        let files = self.art_delete_files();
        if files.is_empty() {
            return;
        }
        let root = self.root.clone();
        let total = files.len();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut moved = Vec::new();
            let mut failed = Vec::new();
            for (i, f) in files.iter().enumerate() {
                match trash::delete(f) {
                    Ok(()) => moved.push(f.clone()),
                    Err(e) => failed.push(format!("{}: {e}", f.file_name().unwrap_or_default().to_string_lossy())),
                }
                let _ = tx.send(JobMsg::Progress(i + 1, total));
            }
            crate::dedupe::prune_empty_dirs(&root, &moved);
            let text = match failed.first() {
                None => format!("곡 {}개를 휴지통으로 옮겼습니다.", moved.len()),
                Some(f) => format!("곡 {}개는 휴지통으로 옮겼고 {}개는 실패했습니다 ({f}).", moved.len(), failed.len()),
            };
            let _ = tx.send(JobMsg::Done(text, !failed.is_empty()));
        });
        self.job_progress = (0, total);
        self.notice = None;
        self.job = Some(rx);
    }

    pub fn is_busy(&self) -> bool {
        self.scan.is_some() || self.job.is_some() || self.search.is_some()
    }

    /// 그림이 없는 앨범을 온라인에서 찾는다
    fn start_search(&mut self) {
        let Some(rows) = &self.arts else { return };
        let queries: Vec<(usize, String, String)> =
            rows.iter().enumerate().filter(|(_, r)| r.a.source.is_none() && !r.a.album.trim().is_empty()).map(|(i, r)| (i, r.a.album.clone(), r.a.artist.clone())).collect();
        if queries.is_empty() {
            self.notice = Some(("앨범 이름이 있는데 그림이 없는 앨범이 없습니다.".into(), false));
            return;
        }
        self.search_progress = (0, queries.len());
        self.notice = None;
        self.search = Some(artsearch::start_batch(queries));
    }

    pub fn pump(&mut self) {
        if let Some(rx) = &self.scan {
            let mut done = false;
            for m in rx.try_iter() {
                match m {
                    ScanMsg::Progress(d, t) => self.progress = (d, t),
                    ScanMsg::Structure(p) => {
                        self.plan = Some(p);
                        done = true;
                    }
                    ScanMsg::Art(a, d) => {
                        self.donors = d;
                        self.picker = None;
                        self.arts = Some(a.into_iter().map(|a| ArtRow { include: a.source.is_some(), a }).collect());
                        done = true;
                    }
                    ScanMsg::Failed(e) => {
                        self.notice = Some((e, true));
                        done = true;
                    }
                }
            }
            if done {
                self.scan = None;
            }
        }
        if let Some(rx) = &self.search {
            let mut done = false;
            for m in rx.try_iter() {
                match m {
                    BatchMsg::Progress(d, t) => self.search_progress = (d, t),
                    BatchMsg::Found(i, f) => {
                        if let Some(r) = self.arts.as_mut().and_then(|rows| rows.get_mut(i)) {
                            r.a.source = Some(ArtSource::Online(f.file, f.from));
                            r.include = true;
                        }
                    }
                    BatchMsg::Done { found, error } => {
                        let total = self.search_progress.1;
                        let mut text = format!("앨범 {total}개 중 {found}개의 그림을 온라인에서 찾았습니다. 미리 보기를 확인한 뒤 넣으세요.");
                        if let Some(e) = &error {
                            text.push_str(&format!(" 일부 검색은 실패했습니다: {e}"));
                        }
                        self.notice = Some((text, error.is_some() && found == 0));
                        done = true;
                    }
                }
            }
            if done {
                self.search = None;
            }
        }
        if let Some(rx) = &self.job {
            let mut done = false;
            for m in rx.try_iter() {
                match m {
                    JobMsg::Progress(d, t) => self.job_progress = (d, t),
                    JobMsg::Done(text, err) => {
                        self.notice = Some((text, err));
                        done = true;
                    }
                }
            }
            if done {
                self.job = None;
                // 결과가 바뀌었으니 다시 훑어 보여 준다
                let root = self.root.clone();
                self.start_scan(&root);
            }
        }
    }

    fn start_scan(&mut self, root: &Path) {
        self.plan = None;
        self.arts = None;
        self.confirm = false;
        self.textures.clear();
        if let Err(e) = organize::guard(root) {
            self.notice = Some((e, true));
            return;
        }
        self.root = root.to_path_buf();
        self.progress = (0, 0);
        self.cancel = Arc::new(AtomicBool::new(false));
        let stop = self.cancel.clone();
        let (tx, rx) = mpsc::channel();
        let (root, mode) = (root.to_path_buf(), self.mode);
        std::thread::spawn(move || {
            let files = match organize::collect(&root, &stop) {
                Ok(f) => f,
                Err(e) => {
                    let _ = tx.send(ScanMsg::Failed(e));
                    return;
                }
            };
            let total = files.len();
            let mut infos: Vec<(AudioInfo, bool)> = Vec::with_capacity(total);
            for (i, f) in files.iter().enumerate() {
                if stop.load(Ordering::Relaxed) {
                    let _ = tx.send(ScanMsg::Failed("중지했습니다.".into()));
                    return;
                }
                let info = audio::read(f, false);
                let has_art = mode == Mode::Art && audio::embedded_cover(f).is_some();
                infos.push((info, has_art));
                let _ = tx.send(ScanMsg::Progress(i + 1, total));
            }
            let msg = match mode {
                Mode::Structure => ScanMsg::Structure(organize::plan(&root, &infos.into_iter().map(|(i, _)| i).collect::<Vec<_>>())),
                Mode::Art => ScanMsg::Art(organize::plan_art(&infos), organize::donors(&infos)),
            };
            let _ = tx.send(msg);
        });
        self.scan = Some(rx);
    }

    fn start_organize(&mut self) {
        let Some(plan) = self.plan.clone() else { return };
        self.confirm = false;
        self.notice = None;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let out = organize::run(&plan, |d, t| {
                let _ = tx.send(JobMsg::Progress(d, t));
            });
            let mut text = format!("곡 {}개를 옮겼습니다", out.moved);
            if out.images > 0 {
                text.push_str(&format!(" (앨범 그림 파일 {}개 함께)", out.images));
            }
            text.push_str(". '마지막 정리 되돌리기'로 원래대로 돌릴 수 있습니다.");
            if !out.failed.is_empty() {
                text.push_str(&format!(" {}개는 옮기지 못했습니다: {}", out.failed.len(), out.failed[0].1));
            }
            let _ = tx.send(JobMsg::Done(text, !out.failed.is_empty()));
        });
        self.job_progress = (0, self.plan.as_ref().map_or(0, |p| p.moves.len()));
        self.job = Some(rx);
    }

    fn start_embed(&mut self) {
        let Some(rows) = &self.arts else { return };
        let work: Vec<(ArtSource, Vec<PathBuf>)> =
            rows.iter().filter(|r| r.include).filter_map(|r| Some((r.a.source.clone()?, r.a.missing.clone()))).collect();
        let total: usize = work.iter().map(|w| w.1.len()).sum();
        self.confirm = false;
        self.notice = None;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let (mut done, mut ok, mut failed) = (0, 0, Vec::<String>::new());
            let mut changed: Vec<PathBuf> = Vec::new();
            for (source, files) in work {
                let bytes = source.bytes();
                for f in files {
                    match &bytes {
                        Some(b) => match organize::embed(&f, b) {
                            Ok(()) => {
                                ok += 1;
                                changed.push(f.clone());
                            }
                            Err(e) => failed.push(format!("{}: {e}", f.file_name().unwrap_or_default().to_string_lossy())),
                        },
                        None => failed.push(format!("{}: 그림을 읽지 못했습니다", f.file_name().unwrap_or_default().to_string_lossy())),
                    }
                    done += 1;
                    let _ = tx.send(JobMsg::Progress(done, total));
                }
            }
            organize::record_embedded(&changed);
            let mut text = format!("곡 {ok}개에 앨범 아트를 넣었습니다. '마지막 아트 넣기 되돌리기'로 뺄 수 있습니다.");
            if let Some(first) = failed.first() {
                text.push_str(&format!(" {}개는 실패했습니다 ({first}).", failed.len()));
            }
            let _ = tx.send(JobMsg::Done(text, !failed.is_empty()));
        });
        self.job_progress = (0, total);
        self.job = Some(rx);
    }

    pub fn show(&mut self, ui: &mut egui::Ui, p: &Palette, cfg: &mut Config, cfg_dirty: &mut bool) {
        let ctx = ui.ctx().clone();
        self.handle_art_keys(&ctx);
        if self.confirm_art_delete {
            let files = self.art_delete_files().len();
            let albums = self.arts.iter().flatten().filter(|r| r.include).count();
            let body = format!("고른 {albums}개 앨범의 곡 {files}개를 휴지통으로 옮깁니다. (휴지통에서 되살릴 수 있습니다)");
            match crate::popup::confirm_delete(&ctx, p, "organize-art", "곡을 삭제할까요?", &body, &format!("{files}곡 삭제")) {
                Some(true) => {
                    self.confirm_art_delete = false;
                    self.start_art_delete();
                }
                Some(false) => self.confirm_art_delete = false,
                None => {}
            }
        }
        let pad = egui::Frame::new().inner_margin(egui::Margin { left: 18, right: 18, top: 0, bottom: 10 });
        let dir = if cfg.organize_path.trim().is_empty() { cfg.output_path.clone() } else { cfg.organize_path.clone() };
        egui::Panel::bottom("organize-actions").frame(pad).show(ui, |ui| {
            ui.add_space(6.0);
            self.action_bar(ui, p, dir.trim());
        });
        if self.picker.is_some() {
            self.picker_window(ui.ctx(), p);
        }
        egui::CentralPanel::default().frame(pad).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 10.0;
            theme::card(ui, p, |ui| self.folder_card(ui, p, cfg, cfg_dirty));
            match self.mode {
                Mode::Structure => self.structure_body(ui, p),
                Mode::Art => self.art_body(ui, p),
            }
        });
    }

    fn folder_card(&mut self, ui: &mut egui::Ui, p: &Palette, cfg: &mut Config, cfg_dirty: &mut bool) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("정리할 폴더").font(theme::bold(16.0)).color(p.text));
            ui.add_space(8.0);
            ui.add_enabled_ui(!self.is_busy(), |ui| {
                let before = self.mode;
                ui.selectable_value(&mut self.mode, Mode::Structure, "폴더 구조 정리");
                ui.selectable_value(&mut self.mode, Mode::Art, "앨범 아트 넣기");
                if self.mode != before {
                    self.plan = None;
                    self.arts = None;
                    self.confirm = false;
                    self.notice = None;
                }
            });
        });
        let help = match self.mode {
            Mode::Structure => "곡 파일을 태그대로 '가수/앨범/트랙 제목' 폴더 구조로 옮기고 비게 된 폴더를 지웁니다. 앨범 폴더의 cover.jpg 같은 그림 파일도 따라 옮깁니다. 제목 태그가 없는 파일은 그대로 둡니다. Music 앱이 관리하는 보관함 폴더는 정리하지 않습니다.",
            Mode::Art => "앨범 아트가 없는 곡을 앨범별로 찾아, 같은 앨범 곡의 그림이나 폴더의 cover.jpg 를 곡 파일 태그에 넣습니다. 그림이 없는 앨범은 그림 파일을 직접 고를 수 있습니다.",
        };
        ui.label(RichText::new(help).size(13.5).color(p.muted));
        let mut path = if cfg.organize_path.trim().is_empty() { cfg.output_path.clone() } else { cfg.organize_path.clone() };
        ui.add_enabled_ui(!self.is_busy(), |ui| {
            ui.horizontal(|ui| {
                let r = ui.add(egui::TextEdit::singleline(&mut path).desired_width(ui.available_width() - 170.0));
                let mut changed = r.changed();
                if ui.button("폴더 선택…").clicked() {
                    if let Some(d) = rfd::FileDialog::new().set_title("정리할 폴더 선택").pick_folder() {
                        path = d.to_string_lossy().into_owned();
                        changed = true;
                    }
                }
                if ui.button("열기").clicked() {
                    open_path(Path::new(path.trim()));
                }
                if changed {
                    cfg.organize_path = path.clone();
                    *cfg_dirty = true;
                    self.plan = None;
                    self.arts = None;
                }
            });
        });
    }

    fn rel<'a>(&self, path: &'a Path) -> std::borrow::Cow<'a, str> {
        path.strip_prefix(&self.root).unwrap_or(path).to_string_lossy()
    }

    fn structure_body(&mut self, ui: &mut egui::Ui, p: &Palette) {
        if self.scan.is_some() {
            ui.label(RichText::new("곡 정보를 읽는 중입니다…").color(p.muted));
            return;
        }
        let Some(plan) = &self.plan else { return };
        theme::card(ui, p, |ui| {
            ui.horizontal_wrapped(|ui| {
                theme::chip(ui, &format!("옮길 곡 {}", plan.moves.len()), p.accent, true);
                theme::chip(ui, &format!("이미 제자리 {}", plan.in_place), p.green, false);
                theme::chip(ui, &format!("함께 옮길 그림 {}", plan.images.len()), p.teal, false);
                theme::chip(ui, &format!("태그 없어 그대로 {}", plan.untagged.len()), p.amber, !plan.untagged.is_empty())
                    .on_hover_text("제목 태그가 없는 파일은 어디로 보낼지 알 수 없어 옮기지 않습니다.");
            });
        });
        if plan.moves.is_empty() {
            ui.label(RichText::new("모든 곡이 이미 '가수/앨범/트랙 제목' 구조에 맞게 놓여 있습니다.").color(p.muted));
            return;
        }
        let row_h = 40.0;
        theme::card(ui, p, |ui| {
            ui.set_min_height(ui.available_height());
            egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, row_h, plan.moves.len(), |ui, range| {
                for m in &plan.moves[range] {
                    ui.allocate_ui(egui::vec2(ui.available_width(), row_h), |ui| {
                        ui.vertical(|ui| {
                            ui.add(egui::Label::new(RichText::new(format!("지금   {}", self.rel(&m.from))).size(12.5).color(p.muted)).truncate());
                            ui.add(egui::Label::new(RichText::new(format!("옮길 곳 {}", self.rel(&m.to))).size(13.5).color(p.text)).truncate());
                        });
                    });
                }
            });
        });
    }

    fn art_body(&mut self, ui: &mut egui::Ui, p: &Palette) {
        if self.scan.is_some() {
            ui.label(RichText::new("곡 정보와 앨범 아트를 읽는 중입니다…").color(p.muted));
            return;
        }
        let Some(rows) = &self.arts else { return };
        let songs: usize = rows.iter().filter(|r| r.include && r.a.source.is_some()).map(|r| r.a.missing.len()).sum();
        let no_source = rows.iter().filter(|r| r.a.source.is_none()).count();
        let mut search = false;
        theme::card(ui, p, |ui| {
            ui.horizontal_wrapped(|ui| {
                theme::chip(ui, &format!("아트 없는 앨범 {}", rows.len()), p.accent, true);
                theme::chip(ui, &format!("넣을 곡 {songs}"), p.blue, false);
                theme::chip(ui, &format!("그림을 골라야 하는 앨범 {no_source}"), p.amber, no_source > 0);
                if no_source > 0 {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let r = theme::tinted_button(ui, p.accent, "온라인에서 찾기", egui::vec2(150.0, 32.0)).on_hover_text(
                            "그림이 없는 앨범을 MusicBrainz(Cover Art Archive)와 iTunes 에서 찾습니다. 앨범 이름과 가수가 맞는 표지만 가져오며, 보내는 것은 앨범·가수 이름뿐입니다.",
                        );
                        if r.clicked() {
                            search = true;
                        }
                    });
                }
            });
        });
        if search {
            self.start_search();
        }
        let Some(rows) = &self.arts else { return };
        if rows.is_empty() {
            ui.label(RichText::new("모든 곡에 앨범 아트가 들어 있습니다.").color(p.muted));
            return;
        }
        let del_n = self.art_delete_files().len();
        let mut select_all = None;
        ui.horizontal(|ui| {
            ui.label(RichText::new("선택").font(theme::bold(13.5)).color(p.text));
            if ui.button("전체 선택").on_hover_text("⌘A").clicked() {
                select_all = Some(true);
            }
            if ui.button("전체 해제").on_hover_text("Esc").clicked() {
                select_all = Some(false);
            }
            let r = theme::tinted_button(ui, p.red, &format!("선택한 곡 {del_n}개 삭제"), egui::vec2(170.0, 32.0));
            if r.on_hover_text("고른 앨범 줄의 아트 없는 곡을 휴지통으로 옮깁니다. Delete 키. 누르면 한 번 더 묻습니다.").clicked() && del_n > 0 {
                self.confirm_art_delete = true;
            }
            ui.label(RichText::new("↑↓ 이동 · 스페이스 선택 · ⌘A 전체 · Esc 해제 · Delete 삭제 · 줄을 눌러도 선택").size(12.5).color(p.muted));
        });
        if let (Some(on), Some(rows)) = (select_all, self.arts.as_mut()) {
            rows.iter_mut().for_each(|r| r.include = on);
        }
        let ctx = ui.ctx().clone();
        let has_donors = !self.donors.is_empty();
        let mut open_picker = None;
        let cursor = self.art_cursor;
        let scroll = std::mem::take(&mut self.art_scroll);
        let mut clicked_row = None;
        let Self { arts, textures, root, .. } = self;
        let rows = arts.as_mut().unwrap();
        theme::card(ui, p, |ui| {
            ui.set_min_height(ui.available_height());
            let row_h = 72.0;
            let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]);
            if let (true, Some(c)) = (scroll, cursor) {
                let step = row_h + ui.spacing().item_spacing.y;
                area = area.vertical_scroll_offset((c as f32 * step - 2.0 * step).max(0.0));
            }
            area.show_rows(ui, row_h, rows.len(), |ui, range| {
                let start = range.start;
                for (k, r) in rows[range].iter_mut().enumerate() {
                    let mut buttons_hovered = false;
                    let line = ui.horizontal(|ui| {
                        ui.set_height(64.0);
                        crate::dedupe_ui::keep_mark(ui, p, true, r.include);
                        art_thumb(ui, p, &ctx, textures, r.a.source.as_ref());
                        let text_w = (ui.available_width() - 150.0).max(200.0);
                        ui.vertical(|ui| {
                            ui.set_max_width(text_w);
                            let album = if r.a.album.is_empty() { "(앨범 없음)".to_string() } else { r.a.album.clone() };
                            let who = if r.a.artist.is_empty() { String::new() } else { format!("  ·  {}", r.a.artist) };
                            ui.add(egui::Label::new(RichText::new(format!("{album}{who}")).font(theme::bold(14.0)).color(p.text)).truncate());
                            let source = match &r.a.source {
                                Some(s) => format!("{}  ·  {}", s.label(), source_name(s, root)),
                                None => "그림 없음 - 그림 파일을 고르세요".to_string(),
                            };
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(format!("곡 {}개 중 {}개 아트 없음", r.a.total, r.a.missing.len())).size(12.5).color(p.muted));
                                ui.add(egui::Label::new(RichText::new(source).size(12.5).color(if r.a.source.is_some() { p.muted } else { p.amber })).truncate());
                            });
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if has_donors {
                                let b = ui.button("다른 앨범에서…").on_hover_text("그림이 있는 다른 앨범을 이름이 비슷한 순서로 보여 줍니다. 고른 앨범의 그림을 씁니다.");
                                buttons_hovered |= b.hovered();
                                if b.clicked() {
                                    open_picker = Some((start + k, r.a.album.clone()));
                                }
                            }
                            let b = ui.button("그림 고르기…");
                            buttons_hovered |= b.hovered();
                            if b.clicked() {
                                let start = r.a.missing.first().and_then(|f| f.parent()).map(Path::to_path_buf).unwrap_or_else(|| root.clone());
                                if let Some(f) = rfd::FileDialog::new().set_title("앨범 아트 그림 선택").add_filter("그림", &["jpg", "jpeg", "png"]).set_directory(start).pick_file() {
                                    r.a.source = Some(ArtSource::Picked(f));
                                    r.include = true;
                                }
                            }
                        });
                    });
                    let rect = line.response.rect;
                    if cursor == Some(start + k) {
                        ui.painter().rect_stroke(rect.expand(3.0), 8, egui::Stroke::new(2.0, p.accent), egui::StrokeKind::Outside);
                    }
                    // 줄을 누르면 선택을 켜고 끈다 (단추는 빼고)
                    let pressed = ui.input(|i| i.pointer.primary_clicked() && i.pointer.interact_pos().is_some_and(|pos| rect.contains(pos)));
                    if pressed && !buttons_hovered {
                        r.include = !r.include;
                        clicked_row = Some(start + k);
                    }
                }
            });
        });
        if let Some(row) = clicked_row {
            self.art_cursor = Some(row);
        }
        if let Some((row, album)) = open_picker {
            self.picker = Some((row, album));
        }
    }

    /// 그림이 있는 다른 앨범 목록 창. 고르면 그 앨범 곡의 그림을 이 앨범에 쓴다.
    fn picker_window(&mut self, ctx: &egui::Context, p: &Palette) {
        let Some((row, query)) = self.picker.as_mut() else { return };
        let row = *row;
        let Some(target) = self.arts.as_ref().and_then(|r| r.get(row)).map(|r| (r.a.album.clone(), r.a.artist.clone())) else {
            self.picker = None;
            return;
        };
        let mut chosen: Option<usize> = None;
        let title = format!("다른 앨범의 그림 고르기 - {}", if target.0.is_empty() { "(앨범 없음)" } else { &target.0 });
        let open = crate::popup::native_window(ctx, "organize-art-picker", &title, [640.0, 580.0], p, |ui| {
            ui.horizontal(|ui| {
                ui.label("찾기");
                ui.add(egui::TextEdit::singleline(query).desired_width(ui.available_width()).hint_text("앨범이나 가수 이름"));
            });
            ui.label(RichText::new("이름이 비슷한 앨범부터 보여 줍니다. 고른 앨범의 그림을 이 앨범의 아트 없는 곡에 넣습니다.").size(12.5).color(p.muted));
            let mut order: Vec<(f32, usize)> = self
                .donors
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
            let Self { donors, textures, .. } = self;
            egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, 64.0, order.len(), |ui, range| {
                for &(_, i) in &order[range] {
                    let d = &donors[i];
                    ui.horizontal(|ui| {
                        ui.set_height(60.0);
                        art_thumb(ui, p, ctx, textures, Some(&ArtSource::Sibling(d.file.clone())));
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
            let d = self.donors[i].clone();
            if let Some(r) = self.arts.as_mut().and_then(|rows| rows.get_mut(row)) {
                r.a.source = Some(ArtSource::Borrowed(d.file, d.album));
                r.include = true;
            }
            self.picker = None;
        } else if !open {
            self.picker = None;
        }
    }

    fn action_bar(&mut self, ui: &mut egui::Ui, p: &Palette, dir: &str) {
        if let Some((text, err)) = self.notice.clone() {
            let mut close = false;
            theme::banner(p, if err { p.red } else { p.green }).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.add(egui::Label::new(RichText::new(&text).font(theme::bold(14.0)).color(p.text)).wrap());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        close = ui.small_button("닫기").clicked();
                    });
                });
            });
            if close {
                self.notice = None;
            }
        }
        if self.search.is_some() {
            let (d, t) = self.search_progress;
            let frac = if t == 0 { 0.0 } else { d as f32 / t as f32 };
            ui.add(egui::ProgressBar::new(frac).text(format!("온라인에서 앨범 아트 찾는 중 {d} / {t}")));
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::tinted_button(ui, p.red, "중지", egui::vec2(110.0, 36.0)).clicked() {
                        self.search = None;
                    }
                });
            });
            return;
        }
        if self.job.is_some() {
            let (d, t) = self.job_progress;
            let frac = if t == 0 { 0.0 } else { d as f32 / t as f32 };
            let what = if self.mode == Mode::Structure { "옮기는 중" } else { "앨범 아트 넣는 중" };
            ui.add(egui::ProgressBar::new(frac).text(format!("{what} {d} / {t}")));
            return;
        }
        if self.scan.is_some() {
            let (d, t) = self.progress;
            let frac = if t == 0 { 0.0 } else { d as f32 / t as f32 };
            let label = if t == 0 { "파일 찾는 중…".to_string() } else { format!("곡 정보 읽는 중 {d} / {t}") };
            ui.add(egui::ProgressBar::new(frac).text(label));
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::tinted_button(ui, p.red, "중지", egui::vec2(110.0, 36.0)).clicked() {
                        self.cancel.store(true, Ordering::Relaxed);
                    }
                });
            });
            return;
        }
        let undo_root = organize::last_root();
        ui.horizontal(|ui| {
            if self.confirm_undo {
                let question = match self.mode {
                    Mode::Structure => format!("{} 의 마지막 정리를 되돌릴까요?", undo_root.as_ref().map(|r| r.display().to_string()).unwrap_or_default()),
                    Mode::Art => format!("마지막으로 아트를 넣은 곡 {}개에서 그 앞표지를 뺄까요?", organize::last_embedded().unwrap_or(0)),
                };
                ui.label(RichText::new(question).font(theme::bold(15.0)).color(p.text));
                if theme::tinted_button(ui, p.red, "되돌리기", egui::vec2(110.0, 36.0)).clicked() {
                    self.confirm_undo = false;
                    let result = match self.mode {
                        Mode::Structure => organize::undo().map(|n| format!("파일 {n}개를 원래 자리로 되돌렸습니다.")),
                        Mode::Art => organize::undo_embedded().map(|n| format!("곡 {n}개에서 넣었던 앨범 아트를 뺐습니다.")),
                    };
                    self.notice = Some(match result {
                        Ok(t) => (t, false),
                        Err(e) => (e, true),
                    });
                    if !self.root.as_os_str().is_empty() {
                        let root = self.root.clone();
                        self.start_scan(&root);
                    }
                }
                if theme::ghost_button(ui, p, "취소").clicked() {
                    self.confirm_undo = false;
                }
                return;
            }
            let scanned = match self.mode {
                Mode::Structure => self.plan.is_some(),
                Mode::Art => self.arts.is_some(),
            };
            if self.confirm {
                let question = match self.mode {
                    Mode::Structure => format!("곡 {}개를 새 폴더 구조로 옮길까요?", self.plan.as_ref().map_or(0, |p| p.moves.len())),
                    Mode::Art => "곡 파일의 태그에 앞표지 그림을 넣습니다. 진행할까요? (마지막 한 번은 되돌릴 수 있습니다)".to_string(),
                };
                ui.label(RichText::new(question).font(theme::bold(15.0)).color(p.text));
                if theme::tinted_button(ui, p.red, "진행", egui::vec2(110.0, 36.0)).clicked() {
                    match self.mode {
                        Mode::Structure => self.start_organize(),
                        Mode::Art => self.start_embed(),
                    }
                }
                if theme::ghost_button(ui, p, "취소").clicked() {
                    self.confirm = false;
                }
                return;
            }
            if !scanned {
                let label = if self.mode == Mode::Structure { "정리 미리 보기" } else { "아트 없는 곡 찾기" };
                if theme::primary_button(ui, p, label, !dir.is_empty()).clicked() {
                    self.start_scan(Path::new(dir));
                }
            } else {
                let (label, n) = match self.mode {
                    Mode::Structure => {
                        let n = self.plan.as_ref().map_or(0, |p| p.moves.len());
                        (format!("곡 {n}개 정리하기"), n)
                    }
                    Mode::Art => {
                        let n: usize = self.arts.iter().flatten().filter(|r| r.include && r.a.source.is_some()).map(|r| r.a.missing.len()).sum();
                        (format!("곡 {n}개에 아트 넣기"), n)
                    }
                };
                if theme::primary_button(ui, p, &label, n > 0).clicked() {
                    self.confirm = true;
                }
                if theme::ghost_button(ui, p, "다시 검사").clicked() {
                    self.start_scan(Path::new(dir));
                }
            }
            let can_undo = match self.mode {
                Mode::Structure => undo_root.is_some(),
                Mode::Art => organize::last_embedded().is_some(),
            };
            if can_undo {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let label = if self.mode == Mode::Structure { "마지막 정리 되돌리기" } else { "마지막 아트 넣기 되돌리기" };
                    if theme::ghost_button(ui, p, label).clicked() {
                        self.confirm_undo = true;
                    }
                });
            }
        });
    }
}

/// 그림 출처의 짧은 이름 (폴더 기준 상대 경로)
fn source_name(s: &ArtSource, root: &Path) -> String {
    if let ArtSource::Online(_, from) | ArtSource::Borrowed(_, from) = s {
        return from.clone();
    }
    let p = s.path();
    p.strip_prefix(root).unwrap_or(p).to_string_lossy().into_owned()
}

/// 앨범 아트 미리 보기 56px. 처음 보일 때 한 번 읽는다.
fn art_thumb(ui: &mut egui::Ui, p: &Palette, ctx: &egui::Context, textures: &mut HashMap<PathBuf, Option<egui::TextureHandle>>, source: Option<&ArtSource>) {
    match source {
        Some(s) => cover_thumb(ui, p, ctx, textures, s.path(), "없음", || s.bytes()),
        None => cover_thumb(ui, p, ctx, textures, Path::new(""), "없음", || None),
    }
}

/// 그림 미리 보기 56px. key 마다 한 번만 load 로 읽어 텍스처로 둔다. 읽지 못하면 empty 글자를 쓴 빈 칸.
pub(crate) fn cover_thumb(
    ui: &mut egui::Ui,
    p: &Palette,
    ctx: &egui::Context,
    textures: &mut HashMap<PathBuf, Option<egui::TextureHandle>>,
    key: &Path,
    empty: &str,
    load: impl FnOnce() -> Option<Vec<u8>>,
) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(56.0, 56.0), egui::Sense::hover());
    let tex = if key.as_os_str().is_empty() {
        None
    } else {
        textures
            .entry(key.to_path_buf())
            .or_insert_with(|| {
                let bytes = load()?;
                let img = image::load_from_memory(&bytes).ok()?.thumbnail(112, 112).to_rgba8();
                let ci = egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw());
                Some(ctx.load_texture(format!("cover:{}", key.display()), ci, egui::TextureOptions::LINEAR))
            })
            .clone()
    };
    match tex {
        Some(t) => {
            let size = t.size_vec2();
            let scale = 56.0 / size.x.max(size.y);
            let r = egui::Rect::from_center_size(rect.center(), size * scale);
            ui.painter().image(t.id(), r, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
        }
        None => {
            ui.painter().rect_filled(rect, 6, p.muted.gamma_multiply(0.18));
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, empty, egui::FontId::proportional(11.5), p.muted);
        }
    }
}
