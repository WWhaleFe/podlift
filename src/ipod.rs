//! 연결된 아이팟 찾기와 꺼내기. 디스크 모드로 마운트된 아이팟은 /Volumes/<이름> 아래에
//! 숨김 폴더 iPod_Control/Music 을 가진다. 맥 포맷(HFS+)·윈도우 포맷(FAT32) 모두 그대로 읽힌다.
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub struct Ipod {
    /// 화면에 보일 이름 (볼륨 이름 또는 고른 폴더 이름)
    pub name: String,
    /// 볼륨 경로 또는 직접 고른 폴더
    pub mount: PathBuf,
    /// 곡 파일이 든 iPod_Control/Music 폴더
    pub music: PathBuf,
    /// /Volumes 에서 찾은 실제 기기면 true (꺼내기 가능), 직접 고른 폴더면 false
    pub device: bool,
    /// SysInfo 의 모델 번호 (예: xB029). 없으면 빈 문자열
    pub model: String,
}

/// /Volumes 아래에서 아이팟을 모두 찾는다.
pub fn detect() -> Vec<Ipod> {
    let Ok(entries) = std::fs::read_dir("/Volumes") else { return Vec::new() };
    let mut found: Vec<Ipod> = entries
        .flatten()
        .filter_map(|e| {
            let mount = e.path();
            let music = mount.join("iPod_Control").join("Music");
            music.is_dir().then(|| Ipod {
                name: e.file_name().to_string_lossy().into_owned(),
                model: read_model(&mount),
                mount,
                music,
                device: true,
            })
        })
        .collect();
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// 직접 고른 폴더에서 음악 폴더를 찾는다. 아이팟 볼륨, iPod_Control, Music 폴더,
/// 혹은 예전에 통째로 복사해 둔 백업 폴더 어느 것을 골라도 된다.
pub fn from_folder(dir: &Path) -> Option<Ipod> {
    let candidates = [
        dir.join("iPod_Control").join("Music"),
        dir.join("Music"),
        dir.to_path_buf(),
    ];
    let music = candidates.into_iter().find(|p| has_fxx(p))?;
    let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| dir.display().to_string());
    Some(Ipod { name, model: read_model(dir), mount: dir.to_path_buf(), music, device: false })
}

/// 폴더 안에 F00 같은 아이팟식 하위 폴더가 있는지
fn has_fxx(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|rd| {
        rd.flatten().any(|e| {
            let n = e.file_name().to_string_lossy().to_ascii_uppercase();
            n.len() == 3 && n.starts_with('F') && n[1..].chars().all(|c| c.is_ascii_digit()) && e.path().is_dir()
        })
    })
}

/// iPod_Control/Device/SysInfo 의 "ModelNumStr: xB029" 줄
fn read_model(mount: &Path) -> String {
    std::fs::read_to_string(mount.join("iPod_Control").join("Device").join("SysInfo"))
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("ModelNumStr:").map(|v| v.trim().to_string()))
        })
        .unwrap_or_default()
}

/// diskutil 로 아이팟을 안전하게 꺼낸다. 몇 초 걸릴 수 있어 호출하는 쪽에서 스레드로 돌린다.
pub fn eject(mount: &Path) -> Result<(), String> {
    let out = std::process::Command::new("diskutil")
        .arg("eject")
        .arg(mount)
        .output()
        .map_err(|e| format!("diskutil 실행 실패: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if msg.is_empty() { String::from_utf8_lossy(&out.stdout).trim().to_string() } else { msg })
    }
}

/// iTunesDB 서명(hash58)에 쓰는 아이팟 고유 번호(FirewireGuid, 16자리 16진수).
/// 찾는 순서: SysInfo 의 "FirewireGuid:" 줄 → SysInfoExtended 의 FireWireGUID →
/// system_profiler 의 USB 일련번호(아이팟 클래식은 USB 일련번호가 곧 FirewireGuid 다).
pub fn firewire_id(mount: &Path) -> Option<String> {
    let device = mount.join("iPod_Control").join("Device");
    let from_sysinfo = std::fs::read_to_string(device.join("SysInfo")).ok().and_then(|s| {
        s.lines().find_map(|l| l.strip_prefix("FirewireGuid:").map(|v| v.trim().to_string()))
    });
    let from_extended = || {
        let v: plist::Value = plist::from_file(device.join("SysInfoExtended")).ok()?;
        v.as_dictionary()?.get("FireWireGUID")?.as_string().map(str::to_string)
    };
    from_sysinfo
        .or_else(from_extended)
        .or_else(usb_serial)
        .filter(|s| crate::itunesdb::parse_firewire_id(s).is_some())
}

/// system_profiler 에서 이름에 iPod 가 든 USB 기기의 일련번호.
/// macOS 26 부터는 SPUSBDataType 이 비어 있고 SPUSBHostDataType(일련번호 키 USBDeviceKeySerialNumber)에 나온다.
fn usb_serial() -> Option<String> {
    ["SPUSBHostDataType", "SPUSBDataType"].iter().find_map(|t| usb_serial_from(t))
}

fn usb_serial_from(data_type: &str) -> Option<String> {
    let out = std::process::Command::new("system_profiler").args([data_type, "-json"]).output().ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    fn walk(v: &serde_json::Value) -> Option<String> {
        if let Some(items) = v.as_array() {
            return items.iter().find_map(walk);
        }
        let obj = v.as_object()?;
        let name = obj.get("_name").and_then(|n| n.as_str()).unwrap_or("");
        if name.contains("iPod") {
            let serial = ["USBDeviceKeySerialNumber", "serial_num"].iter().find_map(|k| obj.get(*k).and_then(|s| s.as_str()));
            if let Some(s) = serial {
                return Some(s.to_string());
            }
        }
        obj.values().find_map(walk)
    }
    walk(&v)
}

/// 볼륨의 남은 공간(바이트). df 로 읽는다.
pub fn free_space(mount: &Path) -> Option<u64> {
    let out = std::process::Command::new("df").arg("-k").arg(mount).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().nth(1)?;
    let kb: u64 = line.split_whitespace().nth(3)?.parse().ok()?;
    Some(kb * 1024)
}
