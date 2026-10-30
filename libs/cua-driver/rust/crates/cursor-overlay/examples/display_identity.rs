// SPDX-License-Identifier: MIT
//! Render the production native cursor and its identity badge without input.
//! Usage: cargo run -p cursor-overlay --example display_identity -- avatar.png out.png
use base64::{engine::general_purpose::STANDARD, Engine as _};
use cursor_overlay::{render_frame, BadgeMode, CursorConfig, OverlayCommand, RenderStateCore};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let avatar = std::fs::read(&args[1]).expect("avatar PNG");
    let mut core = RenderStateCore::new(CursorConfig {
        cursor_id: "research-agent".into(),
        ..Default::default()
    });
    core.badge_mode_override = Some(BadgeMode::Verbose);
    core.motion.idle_hide_ms = 0.0;
    core.pos = (100.0, 65.0);
    core.apply_command_base(
        OverlayCommand::SetSessionLabel("opengeni-9fce4102-32ae-4d81".into()),
        false,
        false,
    );
    if args.get(4).map(String::as_str) != Some("before") {
        core.apply_command_base(
            OverlayCommand::SetSessionIdentity {
                label: args
                    .get(3)
                    .cloned()
                    .unwrap_or_else(|| "Research agent".into()),
                avatar_png: Some(STANDARD.encode(avatar)),
            },
            false,
            false,
        );
    }
    let pixels = render_frame(&core, 400, 240, 0.0, 0.0, None, 2.0);
    image::save_buffer_with_format(
        &args[2],
        pixels.data(),
        400,
        240,
        image::ColorType::Rgba8,
        image::ImageFormat::Png,
    )
    .expect("render PNG");
}
