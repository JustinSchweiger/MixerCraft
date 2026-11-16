//! GUI framework spike: MixerCraft mixer view in egui/eframe.
//!
//! `--mode active|idle` (animated meters at 60 Hz, or silence),
//! `--seconds N` (quit after N seconds). Prints `first_frame_ms=<n>`.

use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, CornerRadius, Pos2, Rect, Sense, Stroke, Vec2};

const STRIPS: usize = 24;
const BUSES: usize = 8;
const N: usize = STRIPS + BUSES;

struct Strip {
    name: String,
    gain: f32,
    mute: bool,
    solo: bool,
    bus: bool,
}

struct MixerApp {
    start: Instant,
    t0: Instant,
    active: bool,
    quit_after: Option<Duration>,
    first_frame: bool,
    strips: Vec<Strip>,
    levels: [f32; 2 * N],
    last_tick: Instant,
}

fn main() -> eframe::Result {
    let start = Instant::now();
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let active = arg("--mode").as_deref() != Some("idle");
    let seconds: u64 = arg("--seconds").and_then(|s| s.parse().ok()).unwrap_or(0);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("MixerCraft – egui spike")
            .with_inner_size([1680.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native(
        "mixercraft-egui-spike",
        options,
        Box::new(move |cc| {
            style(&cc.egui_ctx);
            Ok(Box::new(MixerApp {
                start,
                t0: Instant::now(),
                active,
                quit_after: (seconds > 0).then(|| Duration::from_secs(seconds)),
                first_frame: false,
                strips: (0..N)
                    .map(|i| Strip {
                        name: if i < STRIPS { format!("In {}", i + 1) } else { format!("Bus {}", i - STRIPS + 1) },
                        gain: 0.909,
                        mute: false,
                        solo: false,
                        bus: i >= STRIPS,
                    })
                    .collect(),
                levels: [0.0; 2 * N],
                last_tick: Instant::now(),
            }))
        }),
    )
}

fn style(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = Color32::from_rgb(0x14, 0x15, 0x18);
    visuals.window_fill = visuals.panel_fill;
    ctx.set_visuals(visuals);
}

impl MixerApp {
    fn tick(&mut self) {
        let t = self.t0.elapsed().as_secs_f32();
        let dt = self.last_tick.elapsed().as_secs_f32().min(0.1);
        self.last_tick = Instant::now();
        for i in 0..N {
            for ch in 0..2 {
                let f = 0.3 + 0.17 * i as f32 + 0.05 * ch as f32;
                let target = (0.55 + 0.4 * (t * f * 6.283).sin() * (t * 1.7 + i as f32).cos()).clamp(0.0, 1.0);
                let k = 2 * i + ch;
                let level = &mut self.levels[k];
                *level = if target > *level { target } else { (*level - dt).max(target) };
            }
        }
    }
}

impl eframe::App for MixerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if !self.first_frame {
            self.first_frame = true;
            println!("first_frame_ms={}", self.start.elapsed().as_millis());
        }
        if let Some(q) = self.quit_after {
            let elapsed = self.t0.elapsed();
            if elapsed >= q {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else if !self.active {
                ctx.request_repaint_after(q - elapsed);
            }
        }
        if self.active {
            self.tick();
            ctx.request_repaint_after(Duration::from_millis(16));
        }
        egui::CentralPanel::default().show(ui, |ui| {
            ui.spacing_mut().item_spacing = Vec2::new(1.0, 4.0);
            ui.horizontal_top(|ui| {
                for i in 0..N {
                    let (l, r) = (self.levels[2 * i], self.levels[2 * i + 1]);
                    strip_ui(ui, &mut self.strips[i], l, r);
                }
            });
        });
    }
}

fn strip_ui(ui: &mut egui::Ui, s: &mut Strip, l: f32, r: f32) {
    let bg = if s.bus { Color32::from_rgb(0x1f, 0x22, 0x27) } else { Color32::from_rgb(0x1c, 0x1e, 0x22) };
    egui::Frame::new().fill(bg).stroke(Stroke::new(1.0, Color32::from_rgb(0x12, 0x13, 0x15))).inner_margin(4.0).show(
        ui,
        |ui| {
            ui.set_width(42.0);
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new(&s.name).size(10.0).color(Color32::from_rgb(0xaa, 0xb0, 0xb8)));
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    fader(ui, &mut s.gain, &format!("{} fader", s.name));
                    meter(ui, l);
                    meter(ui, r);
                });
                ui.label(
                    egui::RichText::new(format!("{:.1} dB", s.gain * 66.0 - 60.0))
                        .size(9.0)
                        .color(Color32::from_rgb(0x7f, 0x86, 0x90)),
                );
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    toggle(ui, "M", &mut s.mute, Color32::from_rgb(0xe0, 0x57, 0x4c));
                    if !s.bus {
                        toggle(ui, "S", &mut s.solo, Color32::from_rgb(0xe3, 0xc2, 0x4f));
                    }
                });
            });
        },
    );
}

const FADER_H: f32 = 300.0;

fn meter(ui: &mut egui::Ui, level: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(5.0, FADER_H), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 0.0, Color32::from_rgb(0x23, 0x26, 0x2b));
    let lit_top = rect.bottom() - rect.height() * level.clamp(0.0, 1.0);
    // Green / yellow / red zones, like the Slint gradient stops.
    let zones = [(0.0, 0.72, Color32::from_rgb(0x2f, 0x9e, 0x55)), (0.72, 0.86, Color32::from_rgb(0xd6, 0xb2, 0x3a)), (0.86, 1.0, Color32::from_rgb(0xd9, 0x48, 0x3a))];
    for (a, b, c) in zones {
        let zb = rect.bottom() - rect.height() * a;
        let zt = (rect.bottom() - rect.height() * b).max(lit_top);
        if zt < zb {
            p.rect_filled(Rect::from_min_max(Pos2::new(rect.left(), zt), Pos2::new(rect.right(), zb)), 0.0, c);
        }
    }
}

fn fader(ui: &mut egui::Ui, value: &mut f32, label: &str) {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(22.0, FADER_H), Sense::click_and_drag());
    if response.clicked() || response.drag_started() {
        response.request_focus();
    }
    if let Some(pos) = response.interact_pointer_pos()
        && response.dragged()
    {
        *value = (1.0 - (pos.y - rect.top() - 6.0) / (rect.height() - 12.0)).clamp(0.0, 1.0);
    }
    if response.has_focus() {
        ui.input(|i| {
            if i.key_pressed(egui::Key::ArrowUp) {
                *value = (*value + 0.02).min(1.0);
            }
            if i.key_pressed(egui::Key::ArrowDown) {
                *value = (*value - 0.02).max(0.0);
            }
        });
    }
    let v = f64::from(*value);
    response.widget_info(|| egui::WidgetInfo::slider(true, v, label));
    let p = ui.painter();
    let track = Rect::from_center_size(rect.center(), Vec2::new(4.0, rect.height()));
    p.rect_filled(track, CornerRadius::same(2), Color32::from_rgb(0x2c, 0x30, 0x36));
    let y = rect.top() + (rect.height() - 12.0) * (1.0 - *value);
    let thumb = Rect::from_min_size(Pos2::new(rect.left(), y), Vec2::new(22.0, 12.0));
    let fill = if response.has_focus() { Color32::from_rgb(0xe6, 0xe8, 0xeb) } else { Color32::from_rgb(0xb9, 0xbd, 0xc4) };
    p.rect_filled(thumb, CornerRadius::same(2), fill);
    p.hline(thumb.center().x - 7.0..=thumb.center().x + 7.0, thumb.center().y, Stroke::new(1.0, Color32::from_rgb(0x1b, 0x1d, 0x21)));
}

fn toggle(ui: &mut egui::Ui, text: &str, on: &mut bool, on_color: Color32) {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(22.0, 18.0), Sense::click());
    if response.clicked() {
        *on = !*on;
    }
    let checked = *on;
    response.widget_info(|| egui::WidgetInfo::selected(egui::Role::CheckBox, true, checked, text));
    let fill = if checked {
        on_color
    } else if response.hovered() {
        Color32::from_rgb(0x35, 0x3a, 0x41)
    } else {
        Color32::from_rgb(0x2b, 0x2f, 0x35)
    };
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(3), fill);
    let color = if checked { Color32::from_rgb(0x10, 0x12, 0x14) } else { Color32::from_rgb(0xc9, 0xcc, 0xd1) };
    p.text(rect.center(), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(10.0), color);
}
