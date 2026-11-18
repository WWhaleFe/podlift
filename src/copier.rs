//! 곡 복사. 백그라운드 스레드에서 한 곡씩 복사하고 결과를 채널로 알린다.
//! 복사 중인 파일은 숨김 임시 이름(.이름.podlift-part)으로 쓰고 다 쓴 뒤에 이름을 바꾸므로,
//! 중간에 멈춰도 덜 쓴 파일이 완성된 곡처럼 남지 않는다.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;

pub struct Job {
    /// 앱이 들고 있는 곡 목록의 위치 (결과를 그 줄에 표시한다)
    pub index: usize,
    pub src: PathBuf,
    pub dest: PathBuf,
    pub size: u64,
}

pub enum Outcome {
    /// 새로 복사함
    Copied,
    /// 같은 크기의 파일이 이미 있어 건너뜀
    Skipped,
    Failed(String),
}

pub enum CopyEvent {
    Begin(usize),
    Done(usize, u64, Outcome),
    /// 모두 끝남. Some(이유) 면 도중에 멈춘 것
    Finished(Option<String>),
}

pub struct Copier {
    rx: Receiver<CopyEvent>,
    cancel: Arc<AtomicBool>,
}

impl Copier {
    pub fn poll(&self) -> Vec<CopyEvent> {
        self.rx.try_iter().collect()
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// music_root 는 연결이 끊겼는지 판단하는 데 쓴다.
pub fn start(jobs: Vec<Job>, music_root: PathBuf) -> Copier {
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let stop = cancel.clone();
    std::thread::spawn(move || {
        for job in jobs {
            if stop.load(Ordering::Relaxed) {
                let _ = tx.send(CopyEvent::Finished(Some("중지했습니다.".into())));
                return;
            }
            let _ = tx.send(CopyEvent::Begin(job.index));
            let outcome = match copy_one(&job.src, &job.dest, job.size) {
                Ok(o) => o,
                Err(e) => Outcome::Failed(e.to_string()),
            };
            let failed = matches!(outcome, Outcome::Failed(_));
            let _ = tx.send(CopyEvent::Done(job.index, job.size, outcome));
            if failed && !music_root.is_dir() {
                let _ = tx.send(CopyEvent::Finished(Some("아이팟 연결이 끊겨 멈췄습니다.".into())));
                return;
            }
        }
        let _ = tx.send(CopyEvent::Finished(None));
    });
    Copier { rx, cancel }
}

fn copy_one(src: &Path, dest: &Path, size: u64) -> std::io::Result<Outcome> {
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // 같은 이름이 있으면: 크기가 같으면 이미 받은 곡으로 보고 건너뛰고, 다르면 "이름 (2)" 처럼 비어 있는 이름을 찾는다.
    let mut target = dest.to_path_buf();
    let mut n = 2;
    while let Ok(meta) = std::fs::metadata(&target) {
        if meta.len() == size {
            return Ok(Outcome::Skipped);
        }
        target = numbered(dest, n);
        n += 1;
    }
    let file_name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let part = target.with_file_name(format!(".{file_name}.podlift-part"));
    let result = (|| {
        std::fs::copy(src, &part)?;
        // 아이팟에 들어간 날짜를 그대로 남긴다 (Finder 에서 '수정일' 정렬이 의미 있게)
        if let Ok(modified) = std::fs::metadata(src).and_then(|m| m.modified()) {
            if let Ok(f) = std::fs::File::options().write(true).open(&part) {
                let _ = f.set_modified(modified);
            }
        }
        std::fs::rename(&part, &target)
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    Ok(Outcome::Copied)
}

/// "01 Song.m4a" → "01 Song (n).m4a"
fn numbered(path: &Path, n: u32) -> PathBuf {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let name = match path.extension() {
        Some(ext) => format!("{stem} ({n}).{}", ext.to_string_lossy()),
        None => format!("{stem} ({n})"),
    };
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_same_size_and_number_different() {
        let dir = std::env::temp_dir().join(format!("podlift-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("ABCD.m4a");
        std::fs::write(&src, b"hello").unwrap();
        let dest = dir.join("out/A/B/01 Song.m4a");

        assert!(matches!(copy_one(&src, &dest, 5).unwrap(), Outcome::Copied));
        assert!(dest.is_file());
        assert!(matches!(copy_one(&src, &dest, 5).unwrap(), Outcome::Skipped));

        std::fs::write(&src, b"different!").unwrap();
        let second = dir.join("out/A/B/01 Song (2).m4a");
        assert!(matches!(copy_one(&src, &dest, 10).unwrap(), Outcome::Copied));
        assert_eq!(std::fs::read(&second).unwrap(), b"different!");
        assert!(matches!(copy_one(&src, &dest, 10).unwrap(), Outcome::Skipped));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
