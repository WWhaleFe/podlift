//! 앱 창 밖으로 옮길 수 있는 별도 창(운영체제 창). 앨범 아트 채우기·다른 앨범 그림 고르기에 쓴다.
use crate::egui;
use crate::theme::{self, Palette};

/// id 마다 창 하나. 이번 프레임에 창을 그리고, 창의 닫기 단추를 눌렀으면 false 를 돌려준다.
pub fn native_window(ctx: &egui::Context, id: &str, title: &str, size: [f32; 2], p: &Palette, add: impl FnOnce(&mut egui::Ui)) -> bool {
    let mut open = true;
    let mut add = Some(add);
    let builder = egui::ViewportBuilder::default().with_title(title).with_inner_size(size).with_min_inner_size([420.0, 300.0]);
    ctx.show_viewport_immediate(egui::ViewportId::from_hash_of(id), builder, |ui, _class| {
        if ui.ctx().input(|i| i.viewport().close_requested()) {
            open = false;
        }
        theme::backdrop(ui.painter(), ui.max_rect(), p);
        egui::Frame::new().inner_margin(14).show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            if let Some(f) = add.take() {
                f(ui);
            }
        });
    });
    open
}

/// 삭제 같은 되돌리기 어려운 작업 전에 띄우는 경고창. 뒤 화면은 누를 수 없다.
/// Some(true) = 삭제를 눌렀음, Some(false) = 취소(취소 단추·Esc·바깥 클릭), None = 아직 떠 있음.
/// 실수로 지우지 않도록 엔터로는 삭제하지 않는다.
pub fn confirm_delete(ctx: &egui::Context, p: &Palette, id: &str, title: &str, body: &str, action: &str) -> Option<bool> {
    confirm(ctx, p, id, title, body, action, p.red)
}

/// 실행 전에 한 번 묻는 창 (삭제가 아닌 일). 답은 confirm_delete 와 같다.
pub fn confirm(ctx: &egui::Context, p: &Palette, id: &str, title: &str, body: &str, action: &str, color: egui::Color32) -> Option<bool> {
    let mut answer = None;
    let modal = egui::Modal::new(egui::Id::unique(("confirm-delete", id))).show(ctx, |ui| {
        ui.set_width(440.0);
        ui.label(egui::RichText::new(title).font(theme::bold(17.0)).color(color));
        ui.add_space(6.0);
        ui.label(egui::RichText::new(body).size(14.0).color(p.text));
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if theme::tinted_button(ui, color, action, egui::vec2(150.0, 36.0)).clicked() {
                answer = Some(true);
            }
            if theme::ghost_button(ui, p, "취소").clicked() {
                answer = Some(false);
            }
        });
    });
    if answer.is_none() && modal.should_close() {
        answer = Some(false);
    }
    answer
}
