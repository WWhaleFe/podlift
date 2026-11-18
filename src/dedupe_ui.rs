//! 중복 정리 탭 화면: 대상(맥 폴더·아이팟) 고르기, 중복 찾기, 묶음별 남길 곡 고르기,
//! 버릴 파일 휴지통 이동(폴더) 또는 아이팟에서 빼기(ipod_dedupe.rs).
use crate::app::{human_bytes, open_path};
use crate::audio::{AudioInfo, Verdict};
use crate::config::Config;
use crate::dedupe::{self, DedupeEvent, Group, Scanner, TrashEvent, Trasher};
use crate::egui;
use crate::ipod::Ipod;
use crate::csvlist;
use crate::ipod_dedupe;
use std::sync::mpsc::{self, Receiver};
use crate::theme::{self, Palette};
use egui::{Color32, RichText, Stroke};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// 곡 한 줄의 높이 (썸네일 48 + 안쪽 여백). 묶음 하나가 한 화면에 들어오게 낮게 둔다.
const ROW_H: f32 = 58.0;
const THUMB: f32 = 48.0;
const THUMB_BIG: f32 = 160.0;

struct UiGroup {
    g: Group,
    /// "이 묶음은 그대로 두기": 건드리지 않는다
    skip: bool,
    /// 곡별 남기기. 처음에는 추천한 곡 하나만.
    keeps: Vec<bool>,
    /// 여러 곡 남기기(체크박스). 꺼져 있으면 라디오 버튼처럼 한 곡만 고른다.
    multi: bool,
    /// '선택한 묶음 전체 삭제'에 쓸 묶음 선택
    picked: bool,
}

impl UiGroup {
    fn new(g: Group, skip: bool) -> Self {
        let keeps = ipod_dedupe::keep_only(&g);
        UiGroup { g, skip, keeps, multi: false, picked: false }
    }

    /// j 번째 곡을 고른다: 라디오면 그 곡만, 여러 곡이면 켜고 끈다
    fn select(&mut self, j: usize) {
        if self.skip || j >= self.keeps.len() {
            return;
        }
        if self.multi {
            self.keeps[j] = !self.keeps[j];
        } else {
            self.keeps.iter_mut().enumerate().for_each(|(i, k)| *k = i == j);
        }
    }

    /// 라디오 ↔ 여러 곡. 라디오로 돌아갈 때는 커서 줄(남기기였다면) → 첫 남기기 → 추천 순으로 한 곡만 남긴다.
    fn set_multi(&mut self, on: bool, cursor: Option<usize>) {
        if on == self.multi {
            return;
        }
        self.multi = on;
        if !on {
            let pick = cursor.filter(|&c| self.keeps.get(c) == Some(&true)).or_else(|| self.keeps.iter().position(|k| *k)).unwrap_or(self.g.keep);
            self.keeps.iter_mut().enumerate().for_each(|(i, k)| *k = i == pick);
        }
    }
}

/// 스페이스를 두 번 빠르게 누른 것으로 볼 간격 (초)
const DOUBLE_SPACE: f64 = 0.4;

struct Notice {
    text: String,
    color_err: bool,
    failed: Vec<(PathBuf, String)>,
}

/// CSV 내보내기·불러오기 작업 소식
enum CsvMsg {
    /// (읽은 수, 전체, 하는 일)
    Progress(usize, usize, &'static str),
    Exported(Result<(PathBuf, usize), String>),
    Imported(Result<(Vec<(Group, Vec<usize>)>, csvlist::ImportReport), String>),
}

/// 무엇을 정리하나
#[derive(Clone, Copy, PartialEq)]
enum Target {
    Folder,
    Ipod,
}

pub struct DedupeTab {
    target: Target,
    /// 마지막으로 훑은 대상이 아이팟이면 그 마운트 위치
    scanned_ipod: Option<PathBuf>,
    ipod_job: Option<ipod_dedupe::Job>,
    /// 아이팟 곡 DB 를 고쳤음 (넣기 탭이 DB 를 다시 읽게 한다)
    ipod_changed: bool,
    /// 마지막으로 훑은 폴더 (상대 경로 표시, 빈 폴더 정리 기준)
    scan_root: PathBuf,
    scanner: Option<Scanner>,
    /// (단계, 읽은 수, 전체 수)
    progress: (u8, usize, usize),
    /// 훑기를 한 번이라도 마쳤는지
    scanned: bool,
    scanned_files: usize,
    groups: Vec<UiGroup>,
    /// 묶음 카드의 높이 (보이지 않는 묶음은 이 높이만큼 비워 둔다). 그리면서 실제 값으로 고친다.
    heights: Vec<f32>,
    only_unsure: bool,
    /// 휴지통 이동을 한 번 더 확인하는 중
    confirm: bool,
    trasher: Option<Trasher>,
    trash_progress: (usize, usize),
    trash_moved: HashSet<PathBuf>,
    /// 이번 결과에서 휴지통 이동을 한 번이라도 했는지 (빈 목록 문구를 고른다)
    cleaned: bool,
    trash_failed: Vec<(PathBuf, String)>,
    notice: Option<Notice>,
    /// 경로별 앨범 아트 썸네일 텍스처. 보이는 줄만 만들고 다시 찾을 때 비운다.
    textures: HashMap<PathBuf, egui::TextureHandle>,
    /// 키보드 커서: (묶음, 곡). 화살표로 곡 사이만 오간다.
    cursor: Option<(usize, usize)>,
    /// 커서가 움직였으니 그 줄이 보이게 굴린다
    scroll_to_cursor: bool,
    /// 마지막 스페이스: (시각, 묶음, 누르기 전 남기기 표시). 두 번 누르면 첫 번째를 무르고 모드를 바꾼다.
    last_space: Option<(f64, usize, Vec<bool>)>,
    /// '선택한 묶음 전체 삭제' 경고창을 띄웠는지
    confirm_pick_delete: bool,
    /// CSV 내보내기·불러오기 (진행 중이면 Some), 진행 (읽은 수, 전체, 하는 일)
    csv_job: Option<Receiver<CsvMsg>>,
    csv_progress: (usize, usize, &'static str),
    /// 불러오는 CSV 의 대상 (아이팟이면 마운트)
    csv_ipod: Option<PathBuf>,
}

impl DedupeTab {
    pub fn new() -> Self {
        let mut tab = Self {
            target: Target::Folder,
            scanned_ipod: None,
            ipod_job: None,
            ipod_changed: false,
            scan_root: PathBuf::new(),
            scanner: None,
            progress: (1, 0, 0),
            scanned: false,
            scanned_files: 0,
            groups: Vec::new(),
            heights: Vec::new(),
            only_unsure: false,
            confirm: false,
            trasher: None,
            trash_progress: (0, 0),
            trash_moved: HashSet::new(),
            cleaned: false,
            trash_failed: Vec::new(),
            notice: None,
            textures: HashMap::new(),
            cursor: None,
            scroll_to_cursor: false,
            last_space: None,
            confirm_pick_delete: false,
            csv_job: None,
            csv_progress: (0, 0, ""),
            csv_ipod: None,
        };
        // 개발·점검용: PODLIFT_DEDUPE_DIR 가 있으면 그 폴더로 바로 중복 찾기를 시작한다.
        if let Ok(dir) = std::env::var("PODLIFT_DEDUPE_DIR") {
            if !dir.trim().is_empty() {
                tab.start_scan(PathBuf::from(dir.trim()));
            }
        }
        tab
    }

    pub fn is_busy(&self) -> bool {
        self.scanner.is_some() || self.trasher.is_some() || self.ipod_job.is_some() || self.csv_job.is_some()
    }

    /// 아이팟 곡 DB 를 고쳤으면 한 번 true (넣기 탭이 DB 를 다시 읽는다)
    pub fn take_ipod_changed(&mut self) -> bool {
        std::mem::take(&mut self.ipod_changed)
    }

    /// 매 프레임: 백그라운드 스레드가 보낸 소식을 처리한다 (탭이 안 보여도).
    pub fn pump(&mut self) {
        if let Some(s) = &self.scanner {
            let mut finished = false;
            for ev in s.poll() {
                match ev {
                    DedupeEvent::Progress { stage, done, total } => self.progress = (stage, done, total),
                    DedupeEvent::Done { groups, scanned_files } => {
                        self.heights = groups.iter().map(|g| estimate_height(g.items.len())).collect();
                        // 아이팟은 '확인 필요' 묶음을 처음에는 그대로 둔다 (직접 고른 뒤 체크를 푼다)
                        let ipod = self.scanned_ipod.is_some();
                        self.groups = groups.into_iter().map(|g| UiGroup::new(g.clone(), ipod && g.verdict == Verdict::Unsure)).collect();
                        self.scanned = true;
                        self.scanned_files = scanned_files;
                        finished = true;
                    }
                    DedupeEvent::Failed(msg) => {
                        self.notice = Some(Notice { text: msg.clone(), color_err: msg != dedupe::CANCELLED, failed: Vec::new() });
                        finished = true;
                    }
                }
            }
            if finished {
                self.scanner = None;
            }
        }
        if let Some(rx) = &self.csv_job {
            let mut finished = None;
            for m in rx.try_iter() {
                match m {
                    CsvMsg::Progress(d, t, what) => self.csv_progress = (d, t, what),
                    other => finished = Some(other),
                }
            }
            if let Some(m) = finished {
                self.csv_job = None;
                self.finish_csv(m);
            }
        }
        if let Some(job) = &self.ipod_job {
            if let Some(result) = job.poll() {
                self.ipod_job = None;
                self.finish_ipod(result);
            }
        }
        if let Some(t) = &self.trasher {
            let mut finished = false;
            for ev in t.poll() {
                match ev {
                    TrashEvent::Progress { done, total } => self.trash_progress = (done, total),
                    TrashEvent::Moved(path) => {
                        self.trash_moved.insert(path);
                    }
                    TrashEvent::Failed(path, msg) => self.trash_failed.push((path, msg)),
                    TrashEvent::Finished => finished = true,
                }
            }
            if finished {
                self.trasher = None;
                self.finish_trash();
            }
        }
    }

    fn start_scan(&mut self, root: PathBuf) {
        self.cursor = None;
        self.last_space = None;
        self.scanned_ipod = None;
        self.scan_root = root.clone();
        self.scanner = Some(dedupe::start_scan(root));
        self.progress = (1, 0, 0);
        self.scanned = false;
        self.cleaned = false;
        self.groups.clear();
        self.heights.clear();
        self.textures.clear();
        self.notice = None;
        self.confirm = false;
    }

    /// 버릴 파일 목록: 그대로 두기로 한 묶음을 뺀 나머지에서 남기기 표시가 없는 것
    fn discards(&self) -> Vec<&AudioInfo> {
        let mut out = Vec::new();
        for ug in self.groups.iter().filter(|u| !u.skip) {
            for (j, it) in ug.g.items.iter().enumerate() {
                if !ug.keeps[j] {
                    out.push(it);
                }
            }
        }
        out
    }

    /// 모두 지우기로 한 묶음 수 (확인 문구에 쓴다)
    fn wiped_groups(&self) -> usize {
        self.groups.iter().filter(|u| !u.skip && u.keeps.iter().all(|k| !k)).count()
    }

    fn start_trash(&mut self) {
        let paths: Vec<PathBuf> = self.discards().iter().map(|i| i.path.clone()).collect();
        self.start_trash_paths(paths);
    }

    /// 선택한 묶음의 곡을 모두 지운다 (폴더는 휴지통, 아이팟은 곡 DB 에서 빼기)
    fn delete_picked(&mut self) {
        if self.scanned_ipod.is_some() {
            let Some(mount) = self.scanned_ipod.clone() else { return };
            let groups: Vec<(Group, Option<Vec<bool>>)> =
                self.groups.iter().map(|u| (u.g.clone(), u.picked.then(|| vec![false; u.g.items.len()]))).collect();
            self.notice = None;
            self.ipod_job = Some(ipod_dedupe::start(mount, groups));
        } else {
            let paths: Vec<PathBuf> = self.groups.iter().filter(|u| u.picked).flat_map(|u| u.g.items.iter().map(|i| i.path.clone())).collect();
            self.start_trash_paths(paths);
        }
    }

    fn picked_counts(&self) -> (usize, usize) {
        let picked: Vec<&UiGroup> = self.groups.iter().filter(|u| u.picked).collect();
        (picked.len(), picked.iter().map(|u| u.g.items.len()).sum())
    }

    fn start_trash_paths(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        self.trash_progress = (0, paths.len());
        self.trash_moved.clear();
        self.trash_failed.clear();
        self.notice = None;
        self.confirm = false;
        self.trasher = Some(dedupe::start_trash(paths, self.scan_root.clone()));
    }

    /// 화면에 보이는 묶음 (확인 필요만 보기 반영)
    fn visible_groups(&self) -> Vec<usize> {
        (0..self.groups.len()).filter(|&i| !self.only_unsure || self.groups[i].g.verdict == Verdict::Unsure).collect()
    }

    /// ↑↓: 곡 사이만 오간다(묶음을 넘어가도 곡으로). 스페이스: 고르기, 같은 묶음에서 두 번: 라디오 ↔ 여러 곡.
    /// 위젯보다 먼저 키를 가져가서 다른 단추·체크박스로 포커스가 옮겨 가지 않게 한다.
    fn handle_keys(&mut self, ctx: &egui::Context) {
        let keys = crate::listkeys::take(ctx, true);
        if !keys.any() {
            return;
        }
        let (down, up, space, now) = (keys.down, keys.up, keys.space, ctx.input(|i| i.time));
        let vis = self.visible_groups();
        if vis.is_empty() {
            return;
        }
        // 묶음 선택: X 커서 묶음, ⌘A 전체, Esc 해제, Delete 선택 묶음 삭제(경고창)
        if keys.x {
            let g = self.cursor.map(|c| c.0).filter(|g| vis.contains(g)).unwrap_or(vis[0]);
            self.groups[g].picked = !self.groups[g].picked;
            self.cursor.get_or_insert((g, 0));
        }
        if keys.select_all {
            vis.iter().for_each(|&g| self.groups[g].picked = true);
        }
        if keys.clear {
            self.groups.iter_mut().for_each(|u| u.picked = false);
        }
        if keys.delete && self.groups.iter().any(|u| u.picked) {
            self.confirm_pick_delete = true;
        }
        let cur = self.cursor.filter(|(g, j)| vis.contains(g) && *j < self.groups[*g].g.items.len());
        if down || up {
            self.cursor = Some(match cur {
                None => (vis[0], 0),
                Some((g, j)) => {
                    let pos = vis.iter().position(|&x| x == g).unwrap_or(0);
                    if down {
                        if j + 1 < self.groups[g].g.items.len() {
                            (g, j + 1)
                        } else if let Some(&ng) = vis.get(pos + 1) {
                            (ng, 0)
                        } else {
                            (g, j)
                        }
                    } else if j > 0 {
                        (g, j - 1)
                    } else if pos > 0 {
                        let pg = vis[pos - 1];
                        (pg, self.groups[pg].g.items.len() - 1)
                    } else {
                        (g, j)
                    }
                }
            });
            self.scroll_to_cursor = true;
            self.last_space = None;
        }
        if space {
            let Some((g, j)) = cur.or(self.cursor) else {
                self.cursor = Some((vis[0], 0));
                self.scroll_to_cursor = true;
                return;
            };
            match self.last_space.take() {
                // 같은 묶음에서 두 번: 첫 번째 고르기를 무르고 라디오 ↔ 여러 곡
                Some((t, lg, before)) if lg == g && now - t <= DOUBLE_SPACE => {
                    let ug = &mut self.groups[g];
                    if !ug.skip {
                        ug.keeps = before;
                        let multi = !ug.multi;
                        ug.set_multi(multi, Some(j));
                    }
                }
                _ => {
                    let before = self.groups[g].keeps.clone();
                    self.groups[g].select(j);
                    self.last_space = Some((now, g, before));
                }
            }
            self.cursor = Some((g, j));
        }
    }

    /// 정리할 곳: (CSV 경로의 기준, 곡을 찾을 폴더, 아이팟이면 마운트)
    fn csv_place(&self, cfg: &Config, device: Option<&Ipod>) -> Option<(PathBuf, PathBuf, Option<PathBuf>)> {
        match (self.target, device) {
            (Target::Ipod, Some(d)) => Some((d.mount.clone(), d.mount.join("iPod_Control").join("Music"), Some(d.mount.clone()))),
            (Target::Ipod, None) => None,
            (Target::Folder, _) => {
                let dir = if cfg.dedupe_path.trim().is_empty() { cfg.output_path.clone() } else { cfg.dedupe_path.clone() };
                let dir = PathBuf::from(dir.trim());
                Some((dir.clone(), dir, None))
            }
        }
    }

    /// 전체 곡 목록을 CSV 로 내보낸다 (저장할 곳을 묻는다)
    fn export_csv(&mut self, cfg: &Config, device: Option<&Ipod>) {
        let Some((root, dir, _)) = self.csv_place(cfg, device) else { return };
        let name = if self.target == Target::Ipod { "podlift-아이팟-곡목록.csv" } else { "podlift-곡목록.csv" };
        let Some(out) = rfd::FileDialog::new().set_title("곡 목록 CSV 저장").set_file_name(name).add_filter("CSV", &["csv"]).save_file() else { return };
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let stop = std::sync::atomic::AtomicBool::new(false);
            let result = csvlist::read_all(&dir, &stop, |d, t| {
                let _ = tx.send(CsvMsg::Progress(d, t, "곡 정보 읽는 중"));
            })
            .and_then(|infos| {
                std::fs::write(&out, csvlist::to_csv(&root, &infos)).map_err(|e| format!("저장하지 못했습니다: {e}"))?;
                Ok((out, infos.len()))
            });
            let _ = tx.send(CsvMsg::Exported(result));
        });
        self.notice = None;
        self.csv_progress = (0, 0, "곡 정보 읽는 중");
        self.csv_job = Some(rx);
    }

    /// AI 가 group 을 채운 CSV 를 불러와 중복 묶음을 만든다
    fn import_csv(&mut self, cfg: &Config, device: Option<&Ipod>) {
        let Some((root, dir, ipod)) = self.csv_place(cfg, device) else { return };
        let Some(file) = rfd::FileDialog::new().set_title("group 을 채운 CSV 불러오기").add_filter("CSV", &["csv"]).pick_file() else { return };
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = (|| {
                let bytes = std::fs::read(&file).map_err(|e| format!("CSV 를 읽지 못했습니다: {e}"))?;
                let text = String::from_utf8(bytes).map_err(|_| "CSV 가 UTF-8 이 아닙니다. UTF-8 로 저장해 주세요.".to_string())?;
                let (groups, report) = csvlist::read_groups(&text, &root)?;
                let stop = std::sync::atomic::AtomicBool::new(false);
                let built = csvlist::build_groups(groups, &stop, |d, t| {
                    let _ = tx.send(CsvMsg::Progress(d, t, "묶음의 곡과 앨범 아트 읽는 중"));
                })?;
                Ok((built, report))
            })();
            let _ = tx.send(CsvMsg::Imported(result));
        });
        self.notice = None;
        self.confirm = false;
        self.csv_ipod = ipod;
        self.scan_root = dir;
        self.csv_progress = (0, 0, "CSV 읽는 중");
        self.csv_job = Some(rx);
    }

    fn finish_csv(&mut self, m: CsvMsg) {
        match m {
            CsvMsg::Exported(Ok((path, n))) => {
                self.notice = Some(Notice {
                    text: format!("곡 {n}개 목록을 {} 에 저장했습니다. 'AI 요청문 복사'로 요청문을 복사해 CSV 와 함께 AI 에게 주세요.", path.display()),
                    color_err: false,
                    failed: Vec::new(),
                });
            }
            CsvMsg::Imported(Ok((groups, report))) => {
                self.cursor = None;
                self.last_space = None;
                self.textures.clear();
                self.scanned_ipod = self.csv_ipod.take();
                self.cleaned = false;
                self.scanned = true;
                self.scanned_files = report.rows;
                self.heights = groups.iter().map(|(g, _)| estimate_height(g.items.len())).collect();
                self.groups = groups
                    .into_iter()
                    .map(|(g, marked)| {
                        let mut u = UiGroup::new(g, false);
                        if !marked.is_empty() {
                            u.multi = marked.len() > 1;
                            u.keeps = (0..u.keeps.len()).map(|i| marked.contains(&i)).collect();
                        }
                        u
                    })
                    .collect();
                let mut text = format!("CSV 에서 중복 묶음 {}개를 불러왔습니다 (전체 {}행). 남길 곡을 확인한 뒤 아래 단추로 정리하세요.", report.groups, report.rows);
                if !report.missing.is_empty() {
                    text.push_str(&format!(" 파일을 찾지 못한 {}행은 뺐습니다 (예: {}). 정리할 곳(맥 폴더·아이팟)이 CSV 를 내보낸 곳과 같은지 확인하세요.", report.missing.len(), report.missing[0]));
                }
                let failed = report.missing.iter().map(|m| (PathBuf::from(m), "파일 없음".to_string())).collect();
                self.notice = Some(Notice { text, color_err: false, failed });
            }
            CsvMsg::Exported(Err(e)) | CsvMsg::Imported(Err(e)) => {
                self.notice = Some(Notice { text: e.clone(), color_err: e != dedupe::CANCELLED, failed: Vec::new() });
            }
            CsvMsg::Progress(..) => {}
        }
    }

    /// 'AI 로 중복 찾기' 줄: CSV 내보내기 → AI 요청문 복사 → CSV 불러와 묶기
    fn csv_row(&mut self, ui: &mut egui::Ui, p: &Palette, cfg: &Config, device: Option<&Ipod>) {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("AI로 중복 찾기").font(theme::bold(13.5)).color(p.text)).on_hover_text(
                "가수·그룹·앨범 이름이 한국어/영어로 달라 자동으로 못 찾은 중복을 AI 에게 가려 달라고 할 때 씁니다.\n1) 곡 목록을 CSV 로 내보내고  2) 요청문과 함께 AI 에게 주고  3) AI 가 group 칸을 채운 CSV 를 불러오면 그대로 묶어 줍니다.",
            );
            let ready = !self.is_busy() && (self.target == Target::Folder || device.is_some());
            ui.add_enabled_ui(ready, |ui| {
                if ui.button("① 곡 목록 CSV 내보내기…").on_hover_text("정리할 곳의 곡 전체를 CSV 로 저장합니다 (group·keep 칸은 비어 있음).").clicked() {
                    self.export_csv(cfg, device);
                }
                if ui.button("② AI 요청문 복사").on_hover_text(csvlist::AI_PROMPT).clicked() {
                    ui.ctx().copy_text(csvlist::AI_PROMPT.to_string());
                    self.notice = Some(Notice { text: "AI 요청문을 복사했습니다. AI 에게 CSV 파일과 함께 붙여 넣으세요.".into(), color_err: false, failed: Vec::new() });
                }
                if ui.button("③ CSV 불러와 묶기…").on_hover_text("AI 가 group 칸을 채운 CSV 를 불러옵니다. 같은 group 의 곡이 한 묶음이 되고, keep 이 1 인 곡을 남길 곡으로 고릅니다.").clicked() {
                    self.import_csv(cfg, device);
                }
            });
        });
    }

    /// 아이팟 음악 폴더를 훑는다
    fn start_ipod_scan(&mut self, mount: &Path) {
        self.start_scan(mount.join("iPod_Control").join("Music"));
        self.scanned_ipod = Some(mount.to_path_buf());
    }

    fn start_ipod_remove(&mut self) {
        let Some(mount) = self.scanned_ipod.clone() else { return };
        let groups: Vec<(Group, Option<Vec<bool>>)> = self.groups.iter().map(|u| (u.g.clone(), (!u.skip).then(|| u.keeps.clone()))).collect();
        self.notice = None;
        self.confirm = false;
        self.ipod_job = Some(ipod_dedupe::start(mount, groups));
    }

    fn finish_ipod(&mut self, result: Result<ipod_dedupe::Outcome, String>) {
        match result {
            Ok(out) => {
                self.ipod_changed = true;
                self.drop_items(&out.done.iter().cloned().collect());
                let mut text = format!(
                    "아이팟에서 {}곡을 뺐습니다. 파일은 아이팟 안 .podlift-old 폴더에 옮겨 두었고, '아이팟에 넣기' 탭의 '마지막 작업 되돌리기'로 되돌릴 수 있습니다.",
                    out.removed
                );
                if out.skipped > 0 {
                    text.push_str(&format!(" 남길 곡이 곡 DB 에 없어 {}묶음은 건너뛰었습니다.", out.skipped));
                }
                if !out.failed.is_empty() {
                    text.push_str(&format!(" 파일 {}개는 옮기지 못했습니다(아이팟 화면에서는 빠졌습니다).", out.failed.len()));
                }
                self.notice = Some(Notice { text, color_err: !out.failed.is_empty(), failed: out.failed });
            }
            Err(e) => self.notice = Some(Notice { text: e, color_err: true, failed: Vec::new() }),
        }
    }

    /// 정리한 곡을 목록에서 뺀다. 한 곡만 남은 묶음은 더 이상 중복이 아니다.
    fn drop_items(&mut self, moved: &HashSet<PathBuf>) {
        for ug in &mut self.groups {
            let keep_path = ug.g.items[ug.g.keep].path.clone();
            let flags: Vec<bool> = ug.g.items.iter().map(|i| !moved.contains(&i.path)).collect();
            let mut n = 0;
            ug.keeps.retain(|_| {
                n += 1;
                flags[n - 1]
            });
            ug.g.items.retain(|i| !moved.contains(&i.path));
            ug.g.keep = ug.g.items.iter().position(|i| i.path == keep_path).unwrap_or(0);
        }
        // 한 곡 이하만 남은 묶음, 남기기로 한 곡끼리만 남은 묶음(여러 개 남기기)은 이미 정리를 마쳤다
        let touched = !moved.is_empty();
        self.groups.retain(|u| u.g.items.len() >= 2 && !(touched && !u.skip && u.keeps.iter().all(|k| *k)));
        self.heights = self.groups.iter().map(|u| estimate_height(u.g.items.len())).collect();
        self.cleaned |= !moved.is_empty();
    }

    /// 옮기기가 끝났을 때: 옮긴 곡을 목록에서 빼고 결과를 알린다.
    fn finish_trash(&mut self) {
        let moved = std::mem::take(&mut self.trash_moved);
        let failed = std::mem::take(&mut self.trash_failed);
        self.drop_items(&moved);
        let text = format!("휴지통으로 {}개 옮김, 실패 {}", moved.len(), failed.len());
        self.notice = Some(Notice { text, color_err: !failed.is_empty(), failed });
    }

    /// ipods: 연결된 아이팟 목록, sync_busy: 넣기 탭이 아이팟에 쓰는 중이면 true
    pub fn show(&mut self, ui: &mut egui::Ui, p: &Palette, cfg: &mut Config, cfg_dirty: &mut bool, ipods: &[Ipod], sync_busy: bool) {
        let ctx = ui.ctx().clone();
        // 글자 입력 칸(폴더 경로)에 있을 때는 키를 가져가지 않는다
        if self.scanned && !self.groups.is_empty() && !self.is_busy() && !self.confirm && !self.confirm_pick_delete && !ctx.text_edit_focused() {
            self.handle_keys(&ctx);
        }
        // 경고창: 선택한 묶음 전체 삭제
        if self.confirm_pick_delete {
            let (g, n) = self.picked_counts();
            let body = if self.scanned_ipod.is_some() {
                format!("고른 {g}묶음의 곡 {n}개를 모두 아이팟에서 지웁니다. 이 노래들이 하나도 남지 않고 재생 목록에서도 빠집니다.\n파일은 아이팟 안에 옮겨 두며 '마지막 작업 되돌리기'로 되돌릴 수 있습니다.")
            } else {
                format!("고른 {g}묶음의 곡 {n}개를 모두 휴지통으로 옮깁니다. 이 노래들이 하나도 남지 않습니다.")
            };
            match crate::popup::confirm_delete(&ctx, p, "dedupe-picked", "선택한 묶음을 모두 삭제할까요?", &body, &format!("{n}곡 삭제")) {
                Some(true) => {
                    self.confirm_pick_delete = false;
                    self.delete_picked();
                }
                Some(false) => self.confirm_pick_delete = false,
                None => {}
            }
        }
        // 경고창: 버릴 곡 지우기 (아래 단추)
        if self.confirm {
            let n = self.discards().len();
            let wiped = self.wiped_groups();
            let mut body = if self.target == Target::Ipod {
                format!("남기지 않은 {n}곡을 아이팟에서 뺍니다. 파일은 아이팟 안에 옮겨 두며 '마지막 작업 되돌리기'로 되돌릴 수 있습니다.")
            } else {
                format!("남기지 않은 {n}개 파일을 휴지통으로 옮깁니다.")
            };
            if wiped > 0 {
                body.push_str(&format!("\n'모두 지우기'한 {wiped}묶음은 그 노래가 하나도 남지 않습니다."));
            }
            match crate::popup::confirm_delete(&ctx, p, "dedupe-discards", "중복 곡을 삭제할까요?", &body, &format!("{n}곡 삭제")) {
                Some(true) => {
                    if self.target == Target::Ipod {
                        self.start_ipod_remove();
                    } else {
                        self.start_trash();
                    }
                    self.confirm = false;
                }
                Some(false) => self.confirm = false,
                None => {}
            }
        }
        let pad = egui::Frame::new().inner_margin(egui::Margin { left: 18, right: 18, top: 0, bottom: 10 });
        let dir = if cfg.dedupe_path.trim().is_empty() { cfg.output_path.clone() } else { cfg.dedupe_path.clone() };
        let device = ipods.iter().find(|i| i.device).cloned();
        // 훑은 아이팟이 빠졌으면 결과를 쓸 수 없다
        if let Some(m) = &self.scanned_ipod {
            if self.ipod_job.is_none() && !ipods.iter().any(|i| i.device && &i.mount == m) {
                self.scanned_ipod = None;
                self.scanned = false;
                self.groups.clear();
                self.heights.clear();
            }
        }

        egui::Panel::bottom("dedupe-actions").frame(pad).show(ui, |ui| {
            ui.add_space(6.0);
            self.action_bar(ui, p, dir.trim(), device.as_ref(), sync_busy);
        });

        egui::CentralPanel::default().frame(pad).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 10.0;
            theme::card(ui, p, |ui| self.target_card(ui, p, cfg, cfg_dirty, device.as_ref()));
            self.body(ui, p, &ctx);
        });
    }

    fn target_card(&mut self, ui: &mut egui::Ui, p: &Palette, cfg: &mut Config, cfg_dirty: &mut bool, device: Option<&Ipod>) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("정리할 곳").font(theme::bold(16.0)).color(p.text));
            ui.add_space(8.0);
            ui.add_enabled_ui(!self.is_busy(), |ui| {
                let before = self.target;
                ui.selectable_value(&mut self.target, Target::Folder, "맥 폴더");
                ui.selectable_value(&mut self.target, Target::Ipod, "아이팟");
                if self.target != before {
                    // 다른 곳의 결과를 보여 주지 않는다
                    self.scanned = false;
                    self.scanned_ipod = None;
                    self.groups.clear();
                    self.heights.clear();
                    self.notice = None;
                    self.confirm = false;
                }
            });
            // 정리한 뒤에도 남은 중복이 있는지 처음부터 다시 훑는다
            if self.scanned && !self.is_busy() {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::tinted_button(ui, p.accent, "다시 검사", egui::vec2(120.0, 32.0))
                        .on_hover_text("폴더(아이팟)를 처음부터 다시 훑어 남은 중복이 있는지 찾습니다.")
                        .clicked()
                    {
                        let dir = if cfg.dedupe_path.trim().is_empty() { cfg.output_path.clone() } else { cfg.dedupe_path.clone() };
                        match (self.target, device) {
                            (Target::Ipod, Some(d)) => self.start_ipod_scan(&d.mount),
                            (Target::Ipod, None) => {}
                            (Target::Folder, _) => self.start_scan(PathBuf::from(dir.trim())),
                        }
                    }
                });
            }
        });
        // 결과를 보는 동안에는 설명을 숨기고 한 줄로 줄여 묶음이 한 화면에 들어오게 한다
        if self.scanned && !self.groups.is_empty() {
            self.csv_row(ui, p, cfg, device);
            ui.horizontal(|ui| {
                let place = match (self.target, device) {
                    (Target::Ipod, Some(d)) => format!("{}  ·  {}", d.name, d.mount.display()),
                    (Target::Ipod, None) => "연결된 아이팟이 없습니다".to_string(),
                    (Target::Folder, _) => if cfg.dedupe_path.trim().is_empty() { cfg.output_path.clone() } else { cfg.dedupe_path.clone() },
                };
                ui.add(egui::Label::new(RichText::new(place).size(13.0).color(p.muted)).truncate());
                if self.target == Target::Folder && !self.is_busy() && ui.small_button("폴더 바꾸기…").clicked() {
                    if let Some(d) = rfd::FileDialog::new().set_title("정리할 폴더 선택").pick_folder() {
                        cfg.dedupe_path = d.to_string_lossy().into_owned();
                        *cfg_dirty = true;
                        self.start_scan(d);
                    }
                }
            });
            return;
        }
        if self.target == Target::Ipod {
            ui.label(
                RichText::new("아이팟 안에서 앨범과 제목이 같은 곡을 찾아 음질이 더 좋은 것만 남깁니다. 음질로 가리기 어려우면 앨범 아트를 비교합니다. 뺀 곡이 들어 있던 재생 목록에는 남긴 곡이 대신 들어가고, 재생 횟수·별점도 남긴 곡으로 합칩니다. 파일은 지우지 않고 아이팟 안에 옮겨 두므로 '아이팟에 넣기' 탭의 '마지막 작업 되돌리기'로 되돌릴 수 있습니다.")
                    .size(13.5)
                    .color(p.muted),
            );
            let text = match device {
                Some(d) => format!("{}  ·  {}", d.name, d.mount.display()),
                None => "연결된 아이팟이 없습니다. 아이팟을 디스크 모드로 연결하세요.".to_string(),
            };
            ui.label(RichText::new(text).font(theme::bold(14.0)).color(if device.is_some() { p.text } else { p.amber }));
            self.csv_row(ui, p, cfg, device);
            return;
        }
        ui.label(
            RichText::new("앨범과 제목이 같은 곡을 찾아 음질이 더 좋은 것만 남깁니다. 음질로 가리기 어려우면 앨범 아트를 비교합니다. 버리는 파일은 휴지통으로 옮기므로 되살릴 수 있습니다.")
                .size(13.5)
                .color(p.muted),
        );
        let mut path = if cfg.dedupe_path.trim().is_empty() { cfg.output_path.clone() } else { cfg.dedupe_path.clone() };
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
                    cfg.dedupe_path = path.clone();
                    *cfg_dirty = true;
                }
            });
        });
        self.csv_row(ui, p, cfg, device);
    }

    fn body(&mut self, ui: &mut egui::Ui, p: &Palette, ctx: &egui::Context) {
        if self.scanner.is_some() {
            let text = match self.progress {
                (1, _, 0) => "음악 파일을 찾는 중입니다…",
                (1, _, _) => "곡 정보를 읽는 중입니다…",
                _ => "같은 곡의 앨범 아트를 읽는 중입니다…",
            };
            ui.label(RichText::new(text).color(p.muted));
            return;
        }
        if !self.scanned {
            return;
        }
        let discards = self.discards();
        let bytes: u64 = discards.iter().map(|i| i.size).sum();
        let (mut by_quality, mut by_cover, mut same, mut unsure) = (0, 0, 0, 0);
        for u in &self.groups {
            match u.g.verdict {
                Verdict::Quality(_) => by_quality += 1,
                Verdict::Cover(_) => by_cover += 1,
                Verdict::Same => same += 1,
                Verdict::Unsure => unsure += 1,
            }
        }
        let n_discard = discards.len();
        theme::card(ui, p, |ui| {
            ui.horizontal_wrapped(|ui| {
                theme::chip(ui, &format!("중복 묶음 {}", self.groups.len()), p.accent, true);
                theme::chip(ui, &format!("버릴 파일 {n_discard} · {}", human_bytes(bytes)), p.blue, false);
                theme::chip(ui, &format!("음질로 결정 {by_quality}"), p.green, false);
                theme::chip(ui, &format!("앨범 아트로 결정 {by_cover}"), p.teal, false);
                theme::chip(ui, &format!("같은 사본 {same}"), p.teal, false)
                    .on_hover_text("형식·음질·앨범 아트가 모두 같아 어느 것을 남겨도 같은 묶음입니다. 비트레이트가 조금이라도 높은 쪽을 남깁니다.");
                theme::chip(ui, &format!("확인 필요 {unsure}"), p.amber, unsure > 0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.selectable_value(&mut self.only_unsure, true, "확인 필요만");
                    ui.selectable_value(&mut self.only_unsure, false, "전체");
                });
            });
            let (pg, pn) = self.picked_counts();
            ui.horizontal(|ui| {
                ui.label(RichText::new("묶음 선택").font(theme::bold(13.5)).color(p.text));
                if ui.button("전체 선택").on_hover_text("⌘A").clicked() {
                    let vis = self.visible_groups();
                    vis.into_iter().for_each(|g| self.groups[g].picked = true);
                }
                if ui.button("전체 해제").on_hover_text("Esc").clicked() {
                    self.groups.iter_mut().for_each(|u| u.picked = false);
                }
                let r = theme::tinted_button(ui, p.red, &format!("선택한 {pg}묶음 전체 삭제 ({pn}곡)"), egui::vec2(220.0, 32.0));
                if r.on_hover_text("고른 묶음의 곡을 남기지 않고 모두 지웁니다. Delete 키. 누르면 한 번 더 묻습니다.").clicked() && pg > 0 {
                    self.confirm_pick_delete = true;
                }
            });
        });
        if self.groups.is_empty() {
            let text = if !self.cleaned {
                format!("음악 파일 {}개를 살펴봤지만 중복된 곡이 없습니다.", self.scanned_files)
            } else {
                "중복 묶음을 모두 정리했습니다. 위의 '다시 검사'로 남은 중복이 있는지 다시 훑을 수 있습니다.".to_string()
            };
            ui.label(RichText::new(text).color(p.muted));
            return;
        }
        let show_artist = self.scanned_ipod.is_some();
        ui.label(
            RichText::new("↑↓ 곡 이동 · 스페이스 남길 곡 · 스페이스 두 번 한 곡↔여러 곡 · X 묶음 선택 · ⌘A 전체 · Esc 해제 · Delete 묶음 삭제")
                .size(12.0)
                .color(p.muted),
        );
        let scroll = std::mem::take(&mut self.scroll_to_cursor);
        let Self { groups, heights, textures, scan_root, only_unsure, cursor, .. } = self;
        if let Some(clicked) = results(ui, p, ctx, groups, heights, textures, scan_root, *only_unsure, show_artist, *cursor, scroll) {
            *cursor = Some(clicked);
            self.last_space = None;
        }
    }

    fn action_bar(&mut self, ui: &mut egui::Ui, p: &Palette, dir: &str, device: Option<&Ipod>, sync_busy: bool) {
        if let Some(n) = &self.notice {
            let color = if n.color_err { p.red } else if n.text == dedupe::CANCELLED { p.amber } else { p.green };
            let mut close = false;
            theme::banner(p, color).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.add(egui::Label::new(RichText::new(&n.text).font(theme::bold(14.0)).color(p.text)).wrap());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("닫기").clicked() {
                            close = true;
                        }
                    });
                });
                if !n.failed.is_empty() {
                    egui::CollapsingHeader::new(RichText::new("실패한 파일 보기").color(p.text)).show(ui, |ui| {
                        egui::ScrollArea::vertical().max_height(140.0).show(ui, |ui| {
                            for (path, msg) in &n.failed {
                                ui.label(RichText::new(format!("{}  -  {msg}", path.display())).size(12.5).color(p.red));
                            }
                        });
                    });
                }
            });
            if close {
                self.notice = None;
            }
        }

        if self.csv_job.is_some() {
            let (d, t, what) = self.csv_progress;
            let frac = if t == 0 { 0.0 } else { d as f32 / t as f32 };
            ui.add(egui::ProgressBar::new(frac).text(if t == 0 { format!("{what}…") } else { format!("{what} {d} / {t}") }));
            return;
        }
        if self.ipod_job.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new("아이팟 곡 DB 를 고치고 파일을 옮기는 중입니다… 아이팟을 빼지 마세요.").color(p.text));
            });
            return;
        }

        if self.trasher.is_some() {
            let (done, total) = self.trash_progress;
            let frac = if total == 0 { 0.0 } else { done as f32 / total as f32 };
            ui.add(egui::ProgressBar::new(frac).text(format!("휴지통으로 옮기는 중 {done} / {total}")));
            return;
        }

        if self.scanner.is_some() {
            let (stage, done, total) = self.progress;
            let frac = if total == 0 { 0.0 } else { done as f32 / total as f32 };
            let label = match (stage, total) {
                (_, 0) => "파일 찾는 중…".to_string(),
                (1, _) => format!("곡 정보 읽는 중 {done} / {total}"),
                _ => format!("앨범 아트 읽는 중 {done} / {total}"),
            };
            ui.add(egui::ProgressBar::new(frac).text(label));
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::tinted_button(ui, p.red, "중지", egui::vec2(110.0, 36.0)).clicked() {
                        if let Some(s) = &self.scanner {
                            s.cancel();
                        }
                    }
                });
            });
            return;
        }

        let ipod_mode = self.target == Target::Ipod;
        let rescan = |tab: &mut Self| match (ipod_mode, device) {
            (true, Some(d)) => tab.start_ipod_scan(&d.mount),
            (true, None) => {}
            (false, _) => tab.start_scan(PathBuf::from(dir)),
        };
        let can_scan = if ipod_mode { device.is_some() } else { !dir.is_empty() };

        if !self.scanned {
            if theme::primary_button(ui, p, "중복 찾기", can_scan).clicked() {
                rescan(self);
            }
            return;
        }

        let n = self.discards().len();
        ui.horizontal(|ui| {
            let (label, enabled) = if ipod_mode {
                (format!("버릴 {n}곡을 아이팟에서 빼기"), n > 0 && !sync_busy && self.scanned_ipod.is_some())
            } else {
                (format!("버릴 {n}개를 휴지통으로"), n > 0)
            };
            let r = theme::primary_button(ui, p, &label, enabled);
            if ipod_mode && sync_busy {
                r.clone().on_disabled_hover_text("'아이팟에 넣기' 탭에서 아이팟에 쓰는 중입니다.");
            }
            if r.clicked() {
                self.confirm = true;
            }
            if theme::ghost_button(ui, p, "다시 찾기").clicked() {
                rescan(self);
            }
        });
    }
}

/// 곡 n개짜리 묶음 카드의 대략적인 높이 (아직 그려 보지 않았을 때)
fn estimate_height(n: usize) -> f32 {
    24.0 + 34.0 + 30.0 + n as f32 * (ROW_H + 8.0)
}

/// 묶음 목록. 화면에 보이는 묶음만 그리고 나머지는 높이만큼 비워 둔다.
#[allow(clippy::too_many_arguments)]
/// 묶음 목록. 화면에 보이는 묶음만 그리고 나머지는 높이만큼 비워 둔다(커서가 있는 묶음은 늘 그린다).
/// 마우스로 고른 곡이 있으면 (묶음, 곡)을 돌려준다.
#[allow(clippy::too_many_arguments)]
fn results(
    ui: &mut egui::Ui,
    p: &Palette,
    ctx: &egui::Context,
    groups: &mut [UiGroup],
    heights: &mut [f32],
    textures: &mut HashMap<PathBuf, egui::TextureHandle>,
    root: &Path,
    only_unsure: bool,
    show_artist: bool,
    cursor: Option<(usize, usize)>,
    scroll_to_cursor: bool,
) -> Option<(usize, usize)> {
    let mut clicked = None;
    egui::ScrollArea::vertical().auto_shrink([false, false]).show_viewport(ui, |ui, viewport| {
        let spacing = ui.spacing().item_spacing.y;
        let width = ui.available_width();
        let mut y = 0.0;
        for (i, ug) in groups.iter_mut().enumerate() {
            if only_unsure && ug.g.verdict != Verdict::Unsure {
                continue;
            }
            let h = heights[i];
            let here = cursor.filter(|(g, _)| *g == i).map(|(_, j)| j);
            let visible = (y + h >= viewport.min.y - 300.0 && y <= viewport.max.y + 300.0) || (scroll_to_cursor && here.is_some());
            let top = ui.cursor().top();
            if visible {
                let row = ui.push_id(ug.g.key.clone(), |ui| {
                    theme::card(ui, p, |ui| group_card(ui, p, ctx, ug, textures, root, show_artist, here, scroll_to_cursor))
                });
                if let Some(j) = row.inner {
                    clicked = Some((i, j));
                }
                // 키보드로 이 묶음에 왔으면 묶음 전체가 보이게 굴린다 (화면보다 큰 묶음은 커서 줄만)
                if scroll_to_cursor && here.is_some() && row.response.rect.height() <= viewport.height() {
                    ui.scroll_to_rect(row.response.rect, None);
                }
            } else {
                ui.allocate_exact_size(egui::vec2(width, h), egui::Sense::hover());
            }
            heights[i] = ui.cursor().top() - top - spacing;
            y += heights[i] + spacing;
        }
    });
    clicked
}

/// 남기기 표시를 그린다(누를 수 있는 위젯이 아니다. 누르기는 줄 전체가 받는다). 라디오면 원, 여러 곡이면 네모.
pub(crate) fn keep_mark(ui: &mut egui::Ui, p: &Palette, multi: bool, on: bool) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(20.0, 20.0), egui::Sense::hover());
    let c = rect.center();
    let color = if on { p.green } else { p.muted };
    if multi {
        let r = egui::Rect::from_center_size(c, egui::vec2(16.0, 16.0));
        ui.painter().rect_stroke(r, 4, Stroke::new(1.5, color), egui::StrokeKind::Inside);
        if on {
            ui.painter().rect_filled(r.shrink(1.0), 3, p.green);
            let pts = [c + egui::vec2(-4.5, 0.0), c + egui::vec2(-1.0, 3.5), c + egui::vec2(5.0, -3.5)];
            ui.painter().line(pts.to_vec(), Stroke::new(2.0, p.accent_text));
        }
    } else {
        ui.painter().circle_stroke(c, 8.0, Stroke::new(1.5, color));
        if on {
            ui.painter().circle_filled(c, 4.5, p.green);
        }
    }
}

/// 묶음 카드. 줄(앨범 아트 포함)을 누르면 그 곡을 고르고 그 위치를 돌려준다.
#[allow(clippy::too_many_arguments)]
fn group_card(
    ui: &mut egui::Ui,
    p: &Palette,
    ctx: &egui::Context,
    ug: &mut UiGroup,
    textures: &mut HashMap<PathBuf, egui::TextureHandle>,
    root: &Path,
    show_artist: bool,
    cursor: Option<usize>,
    scroll_to_cursor: bool,
) -> Option<usize> {
    let g = &ug.g;
    ui.horizontal(|ui| {
        ui.checkbox(&mut ug.picked, "").on_hover_text("'선택한 묶음 전체 삭제'에 넣습니다 (X 키)");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.checkbox(&mut ug.skip, "이 묶음은 그대로 두기");
            if ug.picked {
                theme::chip(ui, "삭제 선택됨", p.red, true);
            }
            if g.length_mismatch {
                let color = if g.length_spread_ms() > 60_000 { p.red } else { p.amber };
                theme::chip(ui, "길이가 달라 다른 버전일 수 있음", color, true).on_hover_text(format!(
                    "곡 길이가 최대 {} 차이 납니다. 라이브·리믹스 등 다른 버전일 수 있으니 직접 확인하세요.",
                    mmss(g.length_spread_ms())
                ));
            }
            let (text, color) = match g.verdict {
                Verdict::Quality(_) => ("음질로 결정", p.green),
                Verdict::Cover(_) => ("앨범 아트로 결정", p.teal),
                Verdict::Same => ("같은 사본", p.teal),
                Verdict::Unsure => ("확인 필요", p.amber),
            };
            theme::chip(ui, text, color, true);
            let album = if g.album.is_empty() { "(앨범 없음)" } else { g.album.as_str() };
            // 남은 자리를 왼쪽 정렬로 채워 제목이 왼쪽에 붙게 한다
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.add(egui::Label::new(RichText::new(format!("{album} · {}", g.title)).font(theme::bold(15.0)).color(p.text)).truncate());
            });
        });
    });
    // 묶음 단위: 여러 곡 남기기 / 추천대로 / 모두 남기기 / 모두 지우기
    let kept = ug.keeps.iter().filter(|k| **k).count();
    ui.horizontal(|ui| {
        ui.add_enabled_ui(!ug.skip, |ui| {
            let mut multi = ug.multi;
            if ui.checkbox(&mut multi, "여러 곡 남기기").on_hover_text("켜면 곡마다 체크해 여러 곡을 남길 수 있습니다. 키보드에서는 스페이스를 두 번 누릅니다.").changed() {
                ug.set_multi(multi, cursor);
            }
            ui.add_space(8.0);
            if ui.small_button("추천대로").on_hover_text("추천한 곡 하나만 남깁니다.").clicked() {
                ug.multi = false;
                let best = ug.g.keep;
                ug.keeps.iter_mut().enumerate().for_each(|(i, k)| *k = i == best);
            }
            if ui.small_button("모두 남기기").on_hover_text("이 묶음의 곡을 하나도 지우지 않습니다.").clicked() {
                ug.multi = true;
                ug.keeps.iter_mut().for_each(|k| *k = true);
            }
            if ui.small_button("모두 지우기").on_hover_text("이 묶음의 곡을 모두 지웁니다. 이 노래가 하나도 남지 않습니다.").clicked() {
                ug.multi = true;
                ug.keeps.iter_mut().for_each(|k| *k = false);
            }
        });
        let (text, color) = match kept {
            0 => ("모두 지움 - 이 곡이 남지 않습니다".to_string(), p.red),
            1 => ("1곡 남김".to_string(), p.green),
            n => (format!("{n}곡 남김"), p.blue),
        };
        theme::chip(ui, &text, color, kept != 1);
    });
    let mut clicked = None;
    for j in 0..ug.g.items.len() {
        let is_keep = ug.keeps[j];
        let recommended = j == ug.g.keep;
        let at_cursor = cursor == Some(j);
        let (fill, mut stroke) = if ug.skip || !is_keep {
            (Color32::TRANSPARENT, Stroke::new(1.0, p.border))
        } else {
            (p.green.gamma_multiply(0.14), Stroke::new(1.0, p.green.gamma_multiply(0.7)))
        };
        if at_cursor {
            stroke = Stroke::new(2.5, p.accent);
        }
        let mut finder_hovered = false;
        let frame = egui::Frame::new().fill(fill).stroke(stroke).corner_radius(10).inner_margin(6).show(ui, |ui| {
            ui.set_width(ui.available_width());
            if ug.skip || !is_keep {
                ui.set_opacity(0.6);
            }
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        keep_mark(ui, p, ug.multi, is_keep);
                        ui.label(RichText::new("남기기").size(13.0).color(p.text));
                    });
                    if recommended {
                        ui.label(RichText::new("추천").size(11.5).color(p.green));
                    }
                });
                thumbnail(ui, p, ctx, textures, &ug.g.items[j]);
                ui.vertical(|ui| {
                    ui.set_width(ui.available_width());
                    finder_hovered = track_info(ui, p, &ug.g.items[j], root, show_artist);
                });
            });
        });
        let rect = frame.response.rect;
        if at_cursor && scroll_to_cursor {
            ui.scroll_to_rect(rect, None);
        }
        // 줄 어디를 눌러도(앨범 아트 포함) 고른다. 'Finder에서 보기' 단추는 뺀다.
        let pressed = ui.input(|i| i.pointer.primary_clicked() && i.pointer.interact_pos().is_some_and(|pos| rect.contains(pos)));
        if pressed && !finder_hovered && !ug.skip {
            ug.select(j);
            clicked = Some(j);
        }
    }
    clicked
}

/// 썸네일 64px. 마우스를 올리면 160px 크게와 해상도·용량.
fn thumbnail(ui: &mut egui::Ui, p: &Palette, ctx: &egui::Context, textures: &mut HashMap<PathBuf, egui::TextureHandle>, it: &AudioInfo) {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(THUMB, THUMB), egui::Sense::hover());
    let tex = it.cover.as_ref().and_then(|c| c.thumb.as_ref()).map(|(w, h, px)| {
        let tex = textures.entry(it.path.clone()).or_insert_with(|| {
            let img = egui::ColorImage::from_rgba_unmultiplied([*w as usize, *h as usize], px);
            ctx.load_texture(format!("dedupe:{}", it.path.display()), img, egui::TextureOptions::LINEAR)
        });
        (tex.id(), egui::vec2(*w as f32, *h as f32))
    });
    let Some(cover) = &it.cover else {
        ui.painter().rect_filled(rect, 6, p.muted.gamma_multiply(0.18));
        ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, "아트 없음", egui::FontId::proportional(11.5), p.muted);
        return;
    };
    match tex {
        Some((id, size)) => {
            let scale = THUMB / size.x.max(size.y);
            let img = egui::Rect::from_center_size(rect.center(), size * scale);
            ui.painter().image(id, img, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
            let info = format!("{}×{} · {}", cover.width, cover.height, human_bytes(cover.bytes as u64));
            resp.on_hover_ui(|ui| {
                let big = THUMB_BIG / size.x.max(size.y);
                ui.add(egui::Image::new((id, size * big)));
                ui.label(info);
            });
        }
        None => {
            // 앨범 아트는 있지만 그림으로 읽지 못함
            ui.painter().rect_filled(rect, 6, p.muted.gamma_multiply(0.18));
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, "읽기 실패", egui::FontId::proportional(11.5), p.muted);
        }
    }
}

/// 곡 한 줄의 글자: 윗줄은 음질·길이·아트 해상도·크기, 아랫줄은 경로와 Finder 단추.
/// 아이팟 파일은 이름이 뒤섞여 있어 경로 앞에 가수를 붙인다.
fn track_info(ui: &mut egui::Ui, p: &Palette, it: &AudioInfo, root: &Path, show_artist: bool) -> bool {
    let mut finder_hovered = false;
    ui.horizontal(|ui| {
        cell(ui, 190.0, RichText::new(it.quality_label()).font(theme::bold(14.0)).color(p.text));
        cell(ui, 50.0, RichText::new(mmss(it.duration_ms)).size(13.0).color(p.muted));
        let res = match &it.cover {
            Some(c) if c.width > 0 => format!("{}×{}", c.width, c.height),
            _ => "-".to_string(),
        };
        cell(ui, 90.0, RichText::new(res).size(13.0).color(p.muted));
        ui.label(RichText::new(human_bytes(it.size)).size(13.0).color(p.muted));
    });
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let finder = ui.small_button("Finder에서 보기");
            finder_hovered = finder.hovered();
            if finder.clicked() {
                let _ = std::process::Command::new("open").arg("-R").arg(&it.path).spawn();
            }
            let mut rel = it.path.strip_prefix(root).unwrap_or(&it.path).to_string_lossy().into_owned();
            if show_artist && !it.artist.is_empty() {
                rel = format!("{}  ·  {rel}", it.artist);
            }
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.add(egui::Label::new(RichText::new(rel).size(12.5).color(p.muted)).truncate())
                    .on_hover_text(it.path.to_string_lossy());
            });
        });
    });
    finder_hovered
}

/// 폭이 정해진 칸 (넘치면 잘라서 ... 로 줄인다)
fn cell(ui: &mut egui::Ui, width: f32, text: RichText) {
    ui.allocate_ui_with_layout(egui::vec2(width, 20.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.set_width(width);
        ui.add(egui::Label::new(text).truncate());
    });
}

fn mmss(ms: u64) -> String {
    let s = ms / 1000;
    format!("{}:{:02}", s / 60, s % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group3(keep: usize) -> UiGroup {
        let item = |n: &str| AudioInfo { path: PathBuf::from(n), ..Default::default() };
        UiGroup::new(
            Group { key: "k".into(), album: "A".into(), title: "T".into(), items: vec![item("a"), item("b"), item("c")], keep, verdict: Verdict::Same, length_mismatch: false },
            false,
        )
    }

    #[test]
    fn radio_then_multi_then_radio() {
        let mut u = group3(1);
        assert_eq!(u.keeps, [false, true, false]);
        // 라디오: 고른 곡 하나만
        u.select(2);
        assert_eq!(u.keeps, [false, false, true]);
        // 여러 곡: 켜고 끈다
        u.set_multi(true, Some(2));
        u.select(0);
        assert_eq!(u.keeps, [true, false, true]);
        u.select(2);
        assert_eq!(u.keeps, [true, false, false]);
        // 라디오로 돌아가면 커서 줄(남기기였으면) 하나만, 아니면 첫 남기기
        u.select(2);
        u.set_multi(false, Some(2));
        assert_eq!(u.keeps, [false, false, true]);
        u.set_multi(true, None);
        u.keeps = vec![false, false, false];
        u.set_multi(false, Some(0));
        assert_eq!(u.keeps, [false, true, false], "아무것도 없으면 추천");
        // 그대로 두기 묶음은 고르지 않는다
        u.skip = true;
        u.select(0);
        assert_eq!(u.keeps, [false, true, false]);
    }

    #[test]
    fn mmss_format() {
        assert_eq!(mmss(0), "0:00");
        assert_eq!(mmss(245_900), "4:05");
    }
}
