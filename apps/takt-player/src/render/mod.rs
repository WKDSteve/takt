// SPDX-License-Identifier: GPL-3.0-or-later
//! The video surface. For now an empty area of the right shape: the frames
//! come with the NV12 upload and its WGSL shader (task
//! `wgpu-nv12-textur-und-farbraum-shader`), drawn into this area with a paint
//! callback of `egui-wgpu`.

/// Width to height of the surface until a source says otherwise.
pub const ASPECT: f32 = 16.0 / 9.0;

/// The largest rectangle of `aspect` that fits `available`, centred in it.
#[must_use]
pub fn fit(available: egui::Rect, aspect: f32) -> egui::Rect {
    let width = available.width().min(available.height() * aspect);
    let size = egui::vec2(width, width / aspect);
    egui::Rect::from_center_size(available.center(), size)
}

/// Draws the empty surface into all the space `ui` has left.
pub fn video_surface(ui: &mut egui::Ui) {
    let available = ui.available_rect_before_wrap();
    ui.allocate_rect(available, egui::Sense::hover());
    let painter = ui.painter_at(available);
    painter.rect_filled(available, 0.0, egui::Color32::BLACK);
    let surface = fit(available, ASPECT);
    painter.rect_stroke(
        surface,
        0.0,
        egui::Stroke::new(1.0_f32, ui.visuals().weak_text_color()),
        egui::StrokeKind::Inside,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_surface_keeps_its_shape_and_fits() {
        let wide = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1000.0, 300.0));
        let r = fit(wide, ASPECT);
        assert!((r.height() - 300.0).abs() < 0.01);
        assert!((r.width() / r.height() - ASPECT).abs() < 0.001);
        assert!(wide.contains_rect(r));

        let tall = egui::Rect::from_min_size(egui::pos2(10.0, 10.0), egui::vec2(320.0, 900.0));
        let r = fit(tall, ASPECT);
        assert!((r.width() - 320.0).abs() < 0.01);
        assert!((r.center() - tall.center()).length() < 0.01);
    }
}
