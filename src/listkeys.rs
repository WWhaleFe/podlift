//! 목록 화면 공용 키보드: ↑↓ 이동, 스페이스 선택, ⌘A 전체 선택, Esc 전체 해제, Delete 삭제, X 선택(중복 정리).
//! 위젯보다 먼저 키를 가져가서 단추·체크박스로 포커스가 옮겨 가거나 스페이스가 단추를 누르지 않게 한다.
//! 글자 입력 칸에 있을 때는 아무것도 가져가지 않는다.
use crate::egui;
use egui::{Key, Modifiers};

#[derive(Default, Debug)]
pub struct Keys {
    pub up: bool,
    pub down: bool,
    pub space: bool,
    pub select_all: bool,
    pub clear: bool,
    pub delete: bool,
    pub x: bool,
}

impl Keys {
    pub fn any(&self) -> bool {
        self.up || self.down || self.space || self.select_all || self.clear || self.delete || self.x
    }
}

/// 키를 가져간다. with_x 면 X 키도 가져간다.
pub fn take(ctx: &egui::Context, with_x: bool) -> Keys {
    if ctx.text_edit_focused() {
        return Keys::default();
    }
    let k = ctx.input_mut(|i| Keys {
        up: i.consume_key(Modifiers::NONE, Key::ArrowUp),
        down: i.consume_key(Modifiers::NONE, Key::ArrowDown),
        space: i.consume_key(Modifiers::NONE, Key::Space),
        select_all: i.consume_key(Modifiers::COMMAND, Key::A),
        clear: i.consume_key(Modifiers::NONE, Key::Escape),
        delete: i.consume_key(Modifiers::NONE, Key::Delete) | i.consume_key(Modifiers::NONE, Key::Backspace),
        x: with_x && i.consume_key(Modifiers::NONE, Key::X),
    });
    if k.any() {
        if let Some(id) = ctx.memory(|m| m.focused()) {
            ctx.memory_mut(|m| m.surrender_focus(id));
        }
    }
    k
}

/// 커서를 한 칸 옮긴다 (len 개 목록, 끝에서 멈춘다)
pub fn step(cursor: Option<usize>, len: usize, keys: &Keys) -> Option<usize> {
    if len == 0 {
        return None;
    }
    match (cursor, keys.down, keys.up) {
        (None, true, _) | (None, _, true) => Some(0),
        (Some(c), true, _) => Some((c + 1).min(len - 1)),
        (Some(c), _, true) => Some(c.saturating_sub(1)),
        (c, _, _) => c.map(|c| c.min(len - 1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_stays_in_range() {
        let down = Keys { down: true, ..Default::default() };
        let up = Keys { up: true, ..Default::default() };
        assert_eq!(step(None, 3, &down), Some(0));
        assert_eq!(step(Some(2), 3, &down), Some(2));
        assert_eq!(step(Some(0), 3, &up), Some(0));
        assert_eq!(step(Some(5), 3, &Keys::default()), Some(2));
        assert_eq!(step(Some(1), 0, &down), None);
    }
}
