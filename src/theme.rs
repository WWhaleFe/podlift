//! 글래스모피즘 테마. egui 는 창 뒤를 흐리게 할 수 없으므로 창 안에서 흉내 낸다:
//! 차가운 톤의 그라데이션 배경 + 부드러운 색 덩어리(메시로 그린 방사형 그라데이션) 위에
//! 반투명 흰색 카드(얇은 밝은 테두리, 위쪽 하이라이트)를 올린다. 포인트 색은 앱 아이콘과 같은 민트·하늘색.
//! 밝은/어두운 모드 모두 지원하며 색은 전부 `Palette` 에서만 가져온다.
use crate::egui;
use egui::{Color32, CornerRadius, FontFamily, FontId, Margin, Pos2, Rect, RichText, Stroke, Vec2};

#[derive(Clone, Copy)]
#[allow(dead_code)]
pub struct Palette {
    pub dark: bool,
    pub bg_top: Color32,
    pub bg_bottom: Color32,
    /// (색, 중심 위치 비율 x·y, 반지름 비율(창 너비 기준))
    pub blobs: [(Color32, (f32, f32), f32); 3],
    pub card: Color32,
    pub card_strong: Color32,
    pub border: Color32,
    pub border_hi: Color32,
    pub console: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub accent: Color32,
    pub accent_text: Color32,
    // 로그·상태 색
    pub blue: Color32,
    pub green: Color32,
    pub teal: Color32,
    pub amber: Color32,
    pub yellow: Color32,
    pub red: Color32,
}

pub fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            dark,
            bg_top: Color32::from_rgb(0x0B, 0x12, 0x22),
            bg_bottom: Color32::from_rgb(0x12, 0x1C, 0x32),
            blobs: [
                (Color32::from_rgba_unmultiplied(0x2D, 0xD4, 0xBF, 70), (0.12, 0.08), 0.55),
                (Color32::from_rgba_unmultiplied(0x3B, 0x82, 0xF6, 70), (0.88, 0.22), 0.50),
                (Color32::from_rgba_unmultiplied(0x8B, 0x5C, 0xF6, 60), (0.60, 0.98), 0.60),
            ],
            card: Color32::from_rgba_unmultiplied(255, 255, 255, 18),
            card_strong: Color32::from_rgba_unmultiplied(255, 255, 255, 30),
            border: Color32::from_rgba_unmultiplied(255, 255, 255, 40),
            border_hi: Color32::from_rgba_unmultiplied(255, 255, 255, 70),
            console: Color32::from_rgba_unmultiplied(0, 0, 0, 70),
            text: Color32::from_rgb(0xEA, 0xF2, 0xFF),
            muted: Color32::from_rgb(0x9F, 0xB0, 0xC8),
            accent: Color32::from_rgb(0x5E, 0xEA, 0xD4),
            accent_text: Color32::from_rgb(0x06, 0x20, 0x2A),
            blue: Color32::from_rgb(0x7D, 0xD3, 0xFC),
            green: Color32::from_rgb(0x86, 0xEF, 0xAC),
            teal: Color32::from_rgb(0x5E, 0xEA, 0xD4),
            amber: Color32::from_rgb(0xFB, 0xBF, 0x24),
            yellow: Color32::from_rgb(0xFD, 0xE6, 0x8A),
            red: Color32::from_rgb(0xFB, 0x71, 0x85),
        }
    } else {
        Palette {
            dark,
            bg_top: Color32::from_rgb(0xEE, 0xF6, 0xFF),
            bg_bottom: Color32::from_rgb(0xDC, 0xEA, 0xFB),
            blobs: [
                (Color32::from_rgba_unmultiplied(0x2D, 0xD4, 0xBF, 80), (0.12, 0.08), 0.55),
                (Color32::from_rgba_unmultiplied(0x60, 0xA5, 0xFA, 80), (0.88, 0.22), 0.50),
                (Color32::from_rgba_unmultiplied(0xA7, 0x8B, 0xFA, 70), (0.60, 0.98), 0.60),
            ],
            card: Color32::from_rgba_unmultiplied(255, 255, 255, 150),
            card_strong: Color32::from_rgba_unmultiplied(255, 255, 255, 200),
            border: Color32::from_rgba_unmultiplied(255, 255, 255, 220),
            border_hi: Color32::WHITE,
            console: Color32::from_rgba_unmultiplied(255, 255, 255, 170),
            text: Color32::from_rgb(0x0F, 0x1C, 0x33),
            muted: Color32::from_rgb(0x52, 0x62, 0x7A),
            accent: Color32::from_rgb(0x0E, 0xA5, 0xB7),
            accent_text: Color32::WHITE,
            blue: Color32::from_rgb(0x03, 0x69, 0xA1),
            green: Color32::from_rgb(0x15, 0x80, 0x3D),
            teal: Color32::from_rgb(0x0F, 0x76, 0x6E),
            amber: Color32::from_rgb(0xB4, 0x53, 0x09),
            yellow: Color32::from_rgb(0xA1, 0x62, 0x07),
            red: Color32::from_rgb(0xBE, 0x12, 0x3C),
        }
    }
}

pub fn palette_for(ui: &egui::Ui) -> Palette {
    palette(ui.visuals().dark_mode)
}

pub fn bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("bold".into()))
}

/// egui 기본 위젯 색을 유리 톤으로 맞춘다 (밝음/어두움 각각).
pub fn install(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = Vec2::new(10.0, 8.0);
        style.spacing.button_padding = Vec2::new(14.0, 7.0);
        style.spacing.interact_size.y = 30.0;
        style.spacing.scroll.bar_width = 6.0;
        style.spacing.scroll.floating = true;
    });
    for (theme, dark) in [(egui::Theme::Light, false), (egui::Theme::Dark, true)] {
        let p = palette(dark);
        let mut v = if dark { egui::Visuals::dark() } else { egui::Visuals::light() };
        v.panel_fill = Color32::TRANSPARENT; // 배경은 backdrop 으로 직접 그린다
        v.window_fill = if dark { Color32::from_rgb(0x15, 0x1F, 0x36) } else { Color32::from_rgb(0xF2, 0xF7, 0xFF) };
        v.window_stroke = Stroke::new(1.0, p.border);
        v.window_corner_radius = CornerRadius::same(14);
        v.override_text_color = Some(p.text);
        v.hyperlink_color = p.accent;
        v.selection.bg_fill = p.accent.gamma_multiply(0.35);
        v.selection.stroke = Stroke::new(1.0, p.accent);
        v.extreme_bg_color = p.console; // 입력창 배경
        v.faint_bg_color = p.card;
        v.text_cursor.stroke = Stroke::new(2.0, p.accent);
        let r = CornerRadius::same(10);
        let w = &mut v.widgets;
        w.noninteractive.bg_fill = Color32::TRANSPARENT;
        w.noninteractive.weak_bg_fill = Color32::TRANSPARENT;
        w.noninteractive.bg_stroke = Stroke::new(1.0, p.border); // 구분선
        w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
        w.noninteractive.corner_radius = r;
        w.inactive.bg_fill = p.card_strong;
        w.inactive.weak_bg_fill = p.card_strong;
        w.inactive.bg_stroke = Stroke::new(1.0, p.border);
        w.inactive.fg_stroke = Stroke::new(1.0, p.text);
        w.inactive.corner_radius = r;
        w.hovered.bg_fill = p.card_strong.gamma_multiply(1.6);
        w.hovered.weak_bg_fill = p.card_strong.gamma_multiply(1.6);
        w.hovered.bg_stroke = Stroke::new(1.0, p.border_hi);
        w.hovered.fg_stroke = Stroke::new(1.5, p.text);
        w.hovered.corner_radius = r;
        w.active.bg_fill = p.accent.gamma_multiply(0.45);
        w.active.weak_bg_fill = p.accent.gamma_multiply(0.45);
        w.active.bg_stroke = Stroke::new(1.0, p.accent);
        w.active.fg_stroke = Stroke::new(2.0, p.text);
        w.active.corner_radius = r;
        w.open.bg_fill = p.card_strong;
        w.open.weak_bg_fill = p.card_strong;
        w.open.bg_stroke = Stroke::new(1.0, p.border);
        w.open.fg_stroke = Stroke::new(1.0, p.text);
        w.open.corner_radius = r;
        ctx.set_visuals_of(theme, v);
    }
}

/// 창 전체 배경: 세로 그라데이션 + 부드러운 색 덩어리 세 개. 매 프레임 메시 하나로 그린다.
pub fn backdrop(painter: &egui::Painter, rect: Rect, p: &Palette) {
    let mut mesh = egui::Mesh::default();
    let i = mesh.vertices.len() as u32;
    mesh.colored_vertex(rect.left_top(), p.bg_top);
    mesh.colored_vertex(rect.right_top(), p.bg_top);
    mesh.colored_vertex(rect.right_bottom(), p.bg_bottom);
    mesh.colored_vertex(rect.left_bottom(), p.bg_bottom);
    mesh.add_triangle(i, i + 1, i + 2);
    mesh.add_triangle(i, i + 2, i + 3);
    for (color, (fx, fy), fr) in p.blobs {
        let c = Pos2::new(rect.left() + rect.width() * fx, rect.top() + rect.height() * fy);
        let r = rect.width() * fr;
        let center = mesh.vertices.len() as u32;
        mesh.colored_vertex(c, color);
        let n = 48u32;
        let edge = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 0);
        for k in 0..=n {
            let a = k as f32 / n as f32 * std::f32::consts::TAU;
            mesh.colored_vertex(c + Vec2::new(a.cos(), a.sin()) * r, edge);
        }
        for k in 0..n {
            mesh.add_triangle(center, center + 1 + k, center + 2 + k);
        }
    }
    painter.add(egui::Shape::mesh(mesh));
}

/// 유리 카드 프레임
pub fn glass(p: &Palette) -> egui::Frame {
    egui::Frame::new()
        .fill(p.card)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(14)
        .inner_margin(Margin::symmetric(16, 12))
}

/// 유리 카드: 전체 폭, 위쪽 가장자리에 밝은 하이라이트 선
pub fn card<R>(ui: &mut egui::Ui, p: &Palette, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let out = glass(p).show(ui, |ui| {
        ui.set_width(ui.available_width());
        add(ui)
    });
    let r = out.response.rect;
    ui.painter().line_segment(
        [r.left_top() + Vec2::new(14.0, 0.5), r.right_top() + Vec2::new(-14.0, 0.5)],
        Stroke::new(1.0, p.border_hi),
    );
    out.inner
}

/// 카드 안의 더 어두운(또는 더 흰) 속 패널: 로그 콘솔, 점검 결과 등
pub fn well(p: &Palette) -> egui::Frame {
    egui::Frame::new()
        .fill(p.console)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(12)
        .inner_margin(Margin::symmetric(12, 10))
}

/// 색 띠 알림 (성공 초록 / 오류 빨강 / 안내 포인트색)
pub fn banner(p: &Palette, color: Color32) -> egui::Frame {
    let _ = p;
    egui::Frame::new()
        .fill(color.gamma_multiply(0.16))
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.55)))
        .corner_radius(12)
        .inner_margin(Margin::symmetric(14, 10))
}

/// 작은 캡슐 칩: 색 점 + 글자. 개수 표시·상태 표시에 쓴다.
pub fn chip(ui: &mut egui::Ui, text: &str, color: Color32, strong: bool) -> egui::Response {
    let font = if strong { bold(13.0) } else { FontId::proportional(13.0) };
    let galley = ui.painter().layout_no_wrap(text.to_string(), font, color);
    let size = Vec2::new(galley.size().x + 26.0, 24.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        painter.rect_filled(rect, 12, color.gamma_multiply(if strong { 0.18 } else { 0.08 }));
        painter.rect_stroke(rect, 12, Stroke::new(1.0, color.gamma_multiply(if strong { 0.6 } else { 0.25 })), egui::StrokeKind::Inside);
        painter.circle_filled(Pos2::new(rect.left() + 11.0, rect.center().y), 3.5, color);
        painter.galley(Pos2::new(rect.left() + 19.0, rect.center().y - galley.size().y / 2.0), galley, color);
    }
    resp
}

/// 포인트색 큰 버튼 (시작 등)
pub fn primary_button(ui: &mut egui::Ui, p: &Palette, text: &str, enabled: bool) -> egui::Response {
    let btn = egui::Button::new(RichText::new(text).font(bold(16.0)).color(p.accent_text))
        .fill(p.accent)
        .stroke(Stroke::NONE)
        .corner_radius(12)
        .min_size(Vec2::new(180.0, 42.0));
    ui.add_enabled(enabled, btn)
}

/// 유리 톤 보조 버튼 (크기는 기본)
pub fn ghost_button(ui: &mut egui::Ui, p: &Palette, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(text).color(p.text))
            .fill(p.card_strong)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(10),
    )
}

/// 색 있는 보조 버튼 (중지=빨강 등)
pub fn tinted_button(ui: &mut egui::Ui, color: Color32, text: &str, min_size: Vec2) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(text).font(bold(15.0)).color(color))
            .fill(color.gamma_multiply(0.16))
            .stroke(Stroke::new(1.0, color.gamma_multiply(0.6)))
            .corner_radius(12)
            .min_size(min_size),
    )
}

/// 작은 회색 제목 (카드 위 구분용)
#[allow(dead_code)]
pub fn section_title(ui: &mut egui::Ui, p: &Palette, text: &str) {
    ui.label(RichText::new(text).font(bold(13.0)).color(p.muted));
}

/// 분절 탭: 트랙 위에 선택된 칸만 포인트색으로 채운다.
pub fn segments(ui: &mut egui::Ui, p: &Palette, name: &str, labels: &[&str], selected: usize) -> Option<usize> {
    let height = 34.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 10, p.console);
    painter.rect_stroke(rect, 10, Stroke::new(1.0, p.border), egui::StrokeKind::Inside);
    let w = rect.width() / labels.len() as f32;
    let mut clicked = None;
    for (i, label) in labels.iter().enumerate() {
        let seg = Rect::from_min_size(Pos2::new(rect.left() + w * i as f32, rect.top()), Vec2::new(w, height)).shrink(3.0);
        let id = egui::Id::unique(("segment", name, i));
        let resp = ui.interact(seg, id, egui::Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
        if resp.clicked() {
            clicked = Some(i);
        }
        let on = i == selected;
        let painter = ui.painter();
        let (fill, color) = if on {
            (p.accent, p.accent_text)
        } else if resp.hovered() {
            (p.card_strong, p.text)
        } else {
            (Color32::TRANSPARENT, p.muted)
        };
        painter.rect_filled(seg, 8, fill);
        let font = if on { bold(14.0) } else { FontId::proportional(14.0) };
        let galley = painter.layout_no_wrap(label.to_string(), font, color);
        painter.galley(seg.center() - galley.size() / 2.0, galley, color);
    }
    clicked
}

/// 카드 안 얇은 구분선
#[allow(dead_code)]
pub fn divider(ui: &mut egui::Ui, p: &Palette) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), egui::Sense::hover());
    ui.painter().hline(rect.x_range(), rect.center().y, Stroke::new(1.0, p.border));
}
