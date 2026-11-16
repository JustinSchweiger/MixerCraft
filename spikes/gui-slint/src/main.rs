//! GUI framework spike: MixerCraft mixer view in Slint.
//!
//! `--mode active|idle` (animated meters at 60 Hz, or silence),
//! `--seconds N` (quit after N seconds). Prints `first_frame_ms=<n>`.

use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::{Model, ModelRc, SharedString, Timer, TimerMode, VecModel};

slint::slint! {
    #[edition = "1.19"]
    export struct Level { l: float, r: float }
    export struct StripInfo { name: string, gain: float, mute: bool, solo: bool, bus: bool }

    component Meter inherits Rectangle {
        in property <float> level;
        width: 5px;
        background: @linear-gradient(0deg, #2f9e55 0%, #2f9e55 72%, #d6b23a 86%, #d9483a 100%);
        Rectangle {
            y: 0;
            width: parent.width;
            height: parent.height * (1 - clamp(root.level, 0, 1));
            background: #23262b;
        }
    }

    component Fader inherits FocusScope {
        in-out property <float> value;
        in property <string> label;
        callback changed(float);
        width: 22px;
        accessible-role: slider;
        accessible-label: label;
        accessible-value: round(value * 100);
        accessible-value-minimum: 0;
        accessible-value-maximum: 100;
        accessible-value-step: 1;
        accessible-action-increment => { root.set(root.value + 0.02); }
        accessible-action-decrement => { root.set(root.value - 0.02); }
        function set(v: float) { root.value = clamp(v, 0, 1); root.changed(root.value); }
        key-pressed(e) => {
            if (e.text == Key.UpArrow) { root.set(root.value + 0.02); return accept; }
            if (e.text == Key.DownArrow) { root.set(root.value - 0.02); return accept; }
            reject
        }
        Rectangle { x: (parent.width - 4px) / 2; width: 4px; background: #2c3036; border-radius: 2px; }
        Rectangle {
            width: 22px; height: 12px;
            y: (parent.height - self.height) * (1 - root.value);
            background: root.has-focus ? #e6e8eb : #b9bdc4;
            border-radius: 2px;
            Rectangle { height: 1px; width: 14px; background: #1b1d21; }
        }
        TouchArea {
            mouse-cursor: ns-resize;
            pointer-event(e) => { if (e.kind == PointerEventKind.down) { root.focus(); } }
            moved => { if (self.pressed) { root.set(1 - (self.mouse-y - 6px) / (root.height - 12px)); } }
        }
    }

    component ToggleButton inherits Rectangle {
        in property <string> text;
        in property <bool> on;
        in property <color> on-color;
        callback toggled();
        width: 22px; height: 18px;
        border-radius: 3px;
        background: on ? on-color : (touch.has-hover ? #353a41 : #2b2f35);
        accessible-role: checkbox;
        accessible-label: text;
        accessible-checkable: true;
        accessible-checked: on;
        accessible-action-default => { root.toggled(); }
        Text { text: root.text; color: root.on ? #101214 : #c9ccd1; font-size: 10px; font-weight: 700;
               horizontal-alignment: center; vertical-alignment: center; }
        touch := TouchArea { clicked => { root.toggled(); } }
    }

    component Strip inherits Rectangle {
        in property <StripInfo> info;
        in property <Level> level;
        callback gain-changed(float);
        callback mute-toggled();
        callback solo-toggled();
        width: 50px;
        background: info.bus ? #1f2227 : #1c1e22;
        border-width: 1px;
        border-color: #121315;
        VerticalLayout {
            padding: 4px;
            spacing: 4px;
            Text { text: info.name; color: #aab0b8; font-size: 10px; horizontal-alignment: center; overflow: elide; }
            HorizontalLayout {
                alignment: center;
                spacing: 4px;
                Fader { value: info.gain; label: info.name + " fader"; changed(v) => { root.gain-changed(v); } }
                Meter { level: level.l; }
                Meter { level: level.r; }
            }
            Text { text: round((info.gain * 66 - 60) * 10) / 10 + " dB"; color: #7f8690; font-size: 9px; horizontal-alignment: center; }
            HorizontalLayout {
                alignment: center;
                spacing: 2px;
                ToggleButton { text: "M"; on: info.mute; on-color: #e0574c; toggled => { root.mute-toggled(); } }
                if !info.bus: ToggleButton { text: "S"; on: info.solo; on-color: #e3c24f; toggled => { root.solo-toggled(); } }
            }
        }
    }

    export component MixerWindow inherits Window {
        title: "MixerCraft – Slint spike";
        preferred-width: 1680px;
        preferred-height: 420px;
        background: #141518;
        in property <[StripInfo]> strips;
        in property <[Level]> levels;
        callback gain-changed(int, float);
        callback mute-toggled(int);
        callback solo-toggled(int);
        HorizontalLayout {
            padding: 6px;
            spacing: 1px;
            for s[i] in strips: Strip {
                info: s;
                level: levels[i];
                gain-changed(v) => { root.gain-changed(i, v); }
                mute-toggled => { root.mute-toggled(i); }
                solo-toggled => { root.solo-toggled(i); }
            }
        }
    }
}

const STRIPS: usize = 24;
const BUSES: usize = 8;

fn main() {
    let start = Instant::now();
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let active = arg("--mode").as_deref() != Some("idle");
    let seconds: u64 = arg("--seconds").and_then(|s| s.parse().ok()).unwrap_or(0);

    let ui = MixerWindow::new().expect("create window");
    let strips: Rc<VecModel<StripInfo>> = Rc::new(VecModel::from(
        (0..STRIPS + BUSES)
            .map(|i| StripInfo {
                name: if i < STRIPS { SharedString::from(format!("In {}", i + 1)) } else { SharedString::from(format!("Bus {}", i - STRIPS + 1)) },
                gain: 0.909,
                mute: false,
                solo: false,
                bus: i >= STRIPS,
            })
            .collect::<Vec<_>>(),
    ));
    let levels: Rc<VecModel<Level>> = Rc::new(vec![Level { l: 0.0, r: 0.0 }; STRIPS + BUSES].into());
    ui.set_strips(ModelRc::from(strips.clone()));
    ui.set_levels(ModelRc::from(levels.clone()));

    {
        let strips = strips.clone();
        ui.on_gain_changed(move |i, v| {
            let mut s = strips.row_data(i as usize).unwrap();
            s.gain = v;
            strips.set_row_data(i as usize, s);
        });
    }
    {
        let strips = strips.clone();
        ui.on_mute_toggled(move |i| {
            let mut s = strips.row_data(i as usize).unwrap();
            s.mute = !s.mute;
            strips.set_row_data(i as usize, s);
        });
    }
    {
        let strips = strips.clone();
        ui.on_solo_toggled(move |i| {
            let mut s = strips.row_data(i as usize).unwrap();
            s.solo = !s.solo;
            strips.set_row_data(i as usize, s);
        });
    }

    let first = std::cell::Cell::new(false);
    let notifier = ui.window().set_rendering_notifier(move |state, _| {
        if matches!(state, slint::RenderingState::AfterRendering) && !first.replace(true) {
            println!("first_frame_ms={}", start.elapsed().as_millis());
        }
    });
    if notifier.is_err() {
        // Renderer without notifier support: approximate with the first
        // event-loop iteration.
        Timer::single_shot(Duration::ZERO, move || println!("first_frame_ms~={}", start.elapsed().as_millis()));
    }

    let meter_timer = Timer::default();
    if active {
        let levels = levels.clone();
        let t0 = Instant::now();
        let mut state = [0.0f32; 2 * (STRIPS + BUSES)];
        meter_timer.start(TimerMode::Repeated, Duration::from_millis(16), move || {
            let t = t0.elapsed().as_secs_f32();
            for i in 0..STRIPS + BUSES {
                for ch in 0..2 {
                    let f = 0.3 + 0.17 * i as f32 + 0.05 * ch as f32;
                    let target = (0.55 + 0.4 * (t * f * 6.283).sin() * (t * 1.7 + i as f32).cos()).clamp(0.0, 1.0);
                    let k = 2 * i + ch;
                    // Instant attack, ~20 dB/s release, like the real meters.
                    state[k] = if target > state[k] { target } else { (state[k] - 0.016).max(target) };
                }
                levels.set_row_data(i, Level { l: state[2 * i], r: state[2 * i + 1] });
            }
        });
    }

    let quit_timer = Timer::default();
    if seconds > 0 {
        quit_timer.start(TimerMode::SingleShot, Duration::from_secs(seconds), || {
            let _ = slint::quit_event_loop();
        });
    }
    ui.run().expect("run");
}
