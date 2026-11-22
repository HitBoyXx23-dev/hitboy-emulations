//! Console mode: power-on animation and a home screen with a row of consoles above a row of
//! the selected console's games. Arrow keys and Enter drive it; the mouse works too.

use super::*;
use crate::shell::{self, Phase, Row};
use egui::{Key, Pos2, StrokeKind};

fn heading() -> egui::FontFamily {
    egui::FontFamily::Name("heading".into())
}

impl App {
    pub(super) fn shell_ui(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);
        self.shell.tick(now);
        if self.shell.phase == Phase::Boot {
            let pressed = ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Key { pressed: true, .. } | egui::Event::PointerButton { pressed: true, .. })));
            if pressed || !self.shell_pad.is_empty() {
                self.shell.skip();
                self.shell_pad.clear();
            }
        }
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(t().bg)).show(ctx, |ui| {
            if self.shell.phase == Phase::Boot {
                self.boot_screen(ui, now);
            } else {
                self.home_screen(ui);
            }
        });
        if self.shell.phase == Phase::Boot {
            ctx.request_repaint();
        }
    }

    fn boot_screen(&self, ui: &mut Ui, now: f64) {
        let p = self.shell.progress(now);
        let rect = ui.max_rect();
        let a = ((p / 0.35).min(1.0) * ((1.0 - p) / 0.15).min(1.0)).clamp(0.0, 1.0);
        let center = rect.center() - Vec2::new(0.0, 30.0);
        let id = egui::Id::new("brand-mark-tex-big");
        let tex = ui.ctx().data_mut(|d| d.get_temp::<TextureHandle>(id)).or_else(|| {
            let h = crate::logo::texture(ui.ctx(), t(), 256)?;
            ui.ctx().data_mut(|d| d.insert_temp(id, h.clone()));
            Some(h)
        });
        if let Some(tex) = tex {
            let size = Vec2::splat(120.0 + 24.0 * p);
            egui::Image::from_texture(egui::load::SizedTexture::new(tex.id(), size)).tint(Color32::from_white_alpha((a * 255.0) as u8)).paint_at(ui, Rect::from_center_size(center, size));
        }
        let painter = ui.painter();
        painter.text(center + Vec2::new(0.0, 110.0), Align2::CENTER_CENTER, t().full_name(), FontId::new(30.0, heading()), t().text.gamma_multiply(a));
        let bar = Rect::from_center_size(Pos2::new(rect.center().x, rect.bottom() - 90.0), Vec2::new(260.0, 4.0));
        painter.rect_filled(bar, CornerRadius::same(2), t().surface);
        painter.rect_filled(Rect::from_min_size(bar.min, Vec2::new(bar.width() * p, 4.0)), CornerRadius::same(2), t().primary);
        painter.text(Pos2::new(rect.center().x, rect.bottom() - 60.0), Align2::CENTER_CENTER, "Press any key to skip", FontId::proportional(13.0), t().muted);
    }

    fn reload_shell_games(&mut self, c: &'static library::Console) {
        let dir = self.settings.rom_dirs.get(c.id).map(String::as_str);
        let added = self.settings.added_games.get(c.id).cloned().unwrap_or_default();
        self.shell_games = library::scan(c, dir, &added, &self.settings.hidden_games);
    }

    fn home_screen(&mut self, ui: &mut Ui) {
        let consoles = shell::consoles();
        if consoles.is_empty() {
            return;
        }
        self.shell.console = self.shell.console.min(consoles.len() - 1);
        if self.shell_games_for != Some(self.shell.console) {
            self.reload_shell_games(consoles[self.shell.console]);
            self.shell_games_for = Some(self.shell.console);
        }

        // Keyboard.
        let (left, right, up, down, enter, esc) = ui.ctx().input(|i| (i.key_pressed(Key::ArrowLeft), i.key_pressed(Key::ArrowRight), i.key_pressed(Key::ArrowUp), i.key_pressed(Key::ArrowDown), i.key_pressed(Key::Enter), i.key_pressed(Key::Escape)));
        let (mut left, mut right, mut up, mut down, mut enter, mut esc) = (left, right, up, down, enter, esc);
        for b in std::mem::take(&mut self.shell_pad) {
            match b {
                gilrs::Button::DPadLeft => left = true,
                gilrs::Button::DPadRight => right = true,
                gilrs::Button::DPadUp => up = true,
                gilrs::Button::DPadDown => down = true,
                gilrs::Button::South | gilrs::Button::Start => enter = true,
                gilrs::Button::East => esc = true,
                _ => {}
            }
        }
        let moved = left || right || up || down;
        let games = self.shell_games.len();
        let mut launch: Option<usize> = None;
        if left {
            self.shell.step(-1, consoles.len(), games);
        }
        if right {
            self.shell.step(1, consoles.len(), games);
        }
        if down {
            self.shell.down(games);
        }
        if up {
            self.shell.up();
        }
        if enter {
            match self.shell.row {
                Row::Consoles => self.shell.down(games),
                Row::Games => launch = Some(self.shell.game),
            }
        }
        if esc && !self.shell.up() {
            self.tab = Tab::Library;
        }

        let c = consoles[self.shell.console];
        ui.add_space(26.0);
        ui.horizontal(|ui| {
            ui.add_space(30.0);
            ui.vertical(|ui| {
                ui.label(RichText::new(c.name).size(34.0).family(heading()).color(t().text));
                ui.label(RichText::new(c.note).size(13.0).color(t().muted));
            });
        });
        ui.add_space(22.0);

        // Strip of consoles.
        let mut picked_console = None;
        egui::ScrollArea::horizontal().id_salt("shell-consoles").scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden).auto_shrink([false, true]).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add_space(30.0);
                for (i, cc) in consoles.iter().enumerate() {
                    let (rect, resp) = ui.allocate_exact_size(Vec2::new(150.0, 96.0), Sense::click());
                    let selected = i == self.shell.console;
                    let focus = selected && self.shell.row == Row::Consoles;
                    let p = ui.painter();
                    p.rect_filled(rect, CornerRadius::same(14), if selected || resp.hovered() { t().surface_hover } else { t().surface });
                    let stroke = if focus { Stroke::new(2.5_f32, t().primary) } else if selected { Stroke::new(1.0_f32, t().border) } else { Stroke::NONE };
                    p.rect_stroke(rect, CornerRadius::same(14), stroke, StrokeKind::Inside);
                    p.text(rect.center() - Vec2::new(0.0, 12.0), Align2::CENTER_CENTER, cc.short, FontId::new(24.0, heading()), if selected { t().text } else { t().muted });
                    p.text(rect.center() + Vec2::new(0.0, 26.0), Align2::CENTER_CENTER, cc.name, FontId::proportional(12.0), t().muted);
                    if matches!(cc.status, Status::Playable) {
                        p.circle_filled(rect.right_top() + Vec2::new(-12.0, 12.0), 4.0, t().ok);
                    }
                    if selected && moved {
                        resp.scroll_to_me(Some(Align::Center));
                    }
                    if resp.clicked() {
                        picked_console = Some(i);
                    }
                    ui.add_space(8.0);
                }
            });
        });
        if let Some(i) = picked_console {
            self.shell.console = i;
            self.shell.row = Row::Consoles;
            self.shell.game = 0;
        }
        ui.add_space(26.0);

        // Strip of the selected console's games.
        if self.shell_games.is_empty() {
            ui.horizontal(|ui| {
                ui.add_space(30.0);
                ui.label(RichText::new("No games here yet. Add some in the Games tab, then they show up on this screen.").color(t().muted));
            });
        } else {
            let mut clicked_game = None;
            egui::ScrollArea::horizontal().id_salt("shell-games").scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden).auto_shrink([false, true]).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add_space(30.0);
                    for (i, g) in self.shell_games.iter().enumerate() {
                        let (rect, resp) = ui.allocate_exact_size(Vec2::new(210.0, 250.0), Sense::click());
                        let selected = i == self.shell.game;
                        let focus = selected && self.shell.row == Row::Games;
                        let p = ui.painter();
                        p.rect_filled(rect, CornerRadius::same(14), if focus || resp.hovered() { t().surface_hover } else { t().surface });
                        let stroke = if focus { Stroke::new(2.5_f32, t().primary) } else { Stroke::new(1.0_f32, t().border) };
                        p.rect_stroke(rect, CornerRadius::same(14), stroke, StrokeKind::Inside);
                        // Cover art, fitted into the top of the card.
                        let art_box = Rect::from_min_size(rect.min + Vec2::new(10.0, 10.0), Vec2::new(190.0, 150.0));
                        match self.covers.get(ui.ctx(), &g.path, g.console, &g.title).cloned() {
                            Some(tex) => {
                                let s = tex.size_vec2();
                                let k = (art_box.width() / s.x).min(art_box.height() / s.y);
                                egui::Image::from_texture(egui::load::SizedTexture::new(tex.id(), s * k)).paint_at(ui, Rect::from_center_size(art_box.center(), s * k));
                            }
                            None => {
                                if self.covers.is_pending(&g.path) {
                                    ui.ctx().request_repaint_after(Duration::from_millis(250));
                                }
                                ui.painter().text(art_box.center(), Align2::CENTER_CENTER, library::console(g.console).short, FontId::new(28.0, heading()), t().border);
                            }
                        }
                        let p = ui.painter();
                        let inner = Rect::from_min_max(rect.min + Vec2::new(14.0, 168.0), rect.max - Vec2::new(14.0, 34.0));
                        let clip = p.with_clip_rect(inner);
                        let galley = clip.layout(g.title.clone(), FontId::new(15.0, heading()), t().text, inner.width());
                        clip.galley(inner.min, galley, t().text);
                        p.text(rect.left_bottom() + Vec2::new(14.0, -14.0), Align2::LEFT_BOTTOM, format!("{:.1} MB", g.size as f64 / 1_048_576.0), FontId::proportional(12.0), t().muted);
                        if focus {
                            p.text(rect.right_bottom() + Vec2::new(-14.0, -14.0), Align2::RIGHT_BOTTOM, "Enter to play", FontId::proportional(12.0), t().primary);
                            if moved {
                                resp.scroll_to_me(Some(Align::Center));
                            }
                        }
                        if resp.clicked() {
                            clicked_game = Some(i);
                        }
                        ui.add_space(8.0);
                    }
                });
            });
            if let Some(i) = clicked_game {
                // A click on the game that is already selected plays it; otherwise it selects it.
                if self.shell.row == Row::Games && self.shell.game == i {
                    launch = Some(i);
                } else {
                    self.shell.row = Row::Games;
                    self.shell.game = i;
                }
            }
        }

        let rect = ui.max_rect();
        ui.painter().text(Pos2::new(rect.center().x, rect.bottom() - 24.0), Align2::CENTER_CENTER, "Left / Right choose   Down games   Enter play   Esc back", FontId::proportional(13.0), t().muted);

        if let Some(i) = launch {
            if let Some(g) = self.shell_games.get(i) {
                let (path, console) = (g.path.clone(), g.console);
                self.launch_any(&path, console);
            }
        }
    }
}
