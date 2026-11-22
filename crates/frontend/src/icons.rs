//! Vector icon set drawn with the painter, so it scales crisply and takes theme colours.

use eframe::egui::{Color32, Painter, Pos2, Rect, Shape, Stroke, Vec2};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Icon {
    Gamepad,
    Monitor,
    Gear,
    Play,
    Pause,
    Reset,
    Fullscreen,
    Power,
    Folder,
    Plus,
    Search,
    Trash,
    Back,
    Handheld,
    Console,
    Clock,
    Disk,
    Keyboard,
    Volume,
    Muted,
    Phone,
    Tablet,
    Remote,
}

fn arc(center: Pos2, r: f32, from: f32, to: f32) -> Vec<Pos2> {
    let steps = 24;
    (0..=steps)
        .map(|i| {
            let a = from + (to - from) * i as f32 / steps as f32;
            center + Vec2::angled(a) * r
        })
        .collect()
}

/// Paints `icon` centred in `rect` (designed on a 16x16 grid).
pub fn paint(p: &Painter, rect: Rect, icon: Icon, color: Color32) {
    let s = rect.width().min(rect.height()) / 16.0;
    let o = rect.center() - Vec2::splat(8.0 * s);
    let at = |x: f32, y: f32| o + Vec2::new(x * s, y * s);
    let st = Stroke::new(1.5 * s.max(0.8), color);
    let line = |a: Pos2, b: Pos2| p.line_segment([a, b], st);
    let poly = |pts: Vec<Pos2>| p.add(Shape::line(pts, st));
    let rr = |x0: f32, y0: f32, x1: f32, y1: f32, r: f32| {
        p.rect_stroke(Rect::from_min_max(at(x0, y0), at(x1, y1)), r * s, st, eframe::egui::StrokeKind::Middle)
    };
    match icon {
        Icon::Gamepad => {
            rr(1.0, 4.5, 15.0, 12.5, 3.5);
            line(at(4.5, 7.0), at(4.5, 10.0));
            line(at(3.0, 8.5), at(6.0, 8.5));
            p.circle_filled(at(10.5, 9.5), 1.0 * s, color);
            p.circle_filled(at(12.5, 7.5), 1.0 * s, color);
        }
        Icon::Monitor => {
            rr(1.5, 2.5, 14.5, 11.0, 1.5);
            line(at(8.0, 11.0), at(8.0, 13.5));
            line(at(5.0, 13.5), at(11.0, 13.5));
        }
        Icon::Gear => {
            let c = at(8.0, 8.0);
            for i in 0..8 {
                let a = i as f32 * std::f32::consts::FRAC_PI_4;
                line(c + Vec2::angled(a) * 4.5 * s, c + Vec2::angled(a) * 6.8 * s);
            }
            p.circle_stroke(c, 4.6 * s, st);
            p.circle_stroke(c, 1.8 * s, st);
        }
        Icon::Play => {
            p.add(Shape::convex_polygon(vec![at(4.5, 2.5), at(13.5, 8.0), at(4.5, 13.5)], color, Stroke::NONE));
        }
        Icon::Pause => {
            p.rect_filled(Rect::from_min_max(at(4.0, 3.0), at(6.8, 13.0)), 1.0, color);
            p.rect_filled(Rect::from_min_max(at(9.2, 3.0), at(12.0, 13.0)), 1.0, color);
        }
        Icon::Reset => {
            let c = at(8.0, 8.5);
            poly(arc(c, 5.2 * s, -2.4, 3.3));
            let tip = c + Vec2::angled(-2.4) * 5.2 * s;
            line(tip, tip + Vec2::new(3.2 * s, 0.2 * s));
            line(tip, tip + Vec2::new(0.4 * s, 3.2 * s));
        }
        Icon::Fullscreen => {
            for (x, y, dx, dy) in [(2.0, 2.0, 1.0, 1.0), (14.0, 2.0, -1.0, 1.0), (2.0, 14.0, 1.0, -1.0), (14.0, 14.0, -1.0, -1.0)] {
                line(at(x, y), at(x + 4.0 * dx, y));
                line(at(x, y), at(x, y + 4.0 * dy));
            }
        }
        Icon::Power => {
            poly(arc(at(8.0, 8.8), 5.6 * s, -1.0, 4.14));
            line(at(8.0, 1.8), at(8.0, 8.0));
        }
        Icon::Folder => {
            poly(vec![at(1.5, 4.0), at(6.0, 4.0), at(7.5, 5.5), at(14.5, 5.5), at(14.5, 13.0), at(1.5, 13.0), at(1.5, 4.0)]);
        }
        Icon::Plus => {
            line(at(8.0, 2.5), at(8.0, 13.5));
            line(at(2.5, 8.0), at(13.5, 8.0));
        }
        Icon::Search => {
            p.circle_stroke(at(7.0, 7.0), 4.5 * s, st);
            line(at(10.3, 10.3), at(14.0, 14.0));
        }
        Icon::Trash => {
            line(at(2.5, 4.0), at(13.5, 4.0));
            line(at(6.0, 4.0), at(6.5, 2.0));
            line(at(6.5, 2.0), at(9.5, 2.0));
            line(at(9.5, 2.0), at(10.0, 4.0));
            poly(vec![at(3.8, 4.0), at(4.8, 14.0), at(11.2, 14.0), at(12.2, 4.0)]);
        }
        Icon::Back => {
            poly(vec![at(10.0, 3.0), at(5.0, 8.0), at(10.0, 13.0)]);
        }
        Icon::Handheld => {
            rr(3.5, 1.5, 12.5, 14.5, 1.5);
            rr(5.0, 3.0, 11.0, 7.5, 0.5);
            line(at(5.5, 10.5), at(7.5, 10.5));
            line(at(6.5, 9.5), at(6.5, 11.5));
            p.circle_filled(at(10.5, 10.5), 0.9 * s, color);
        }
        Icon::Console => {
            rr(1.5, 5.0, 14.5, 12.5, 1.5);
            line(at(4.0, 8.0), at(10.0, 8.0));
            p.circle_filled(at(12.0, 10.0), 0.9 * s, color);
        }
        Icon::Clock => {
            p.circle_stroke(at(8.0, 8.0), 6.0 * s, st);
            line(at(8.0, 8.0), at(8.0, 4.5));
            line(at(8.0, 8.0), at(10.5, 9.5));
        }
        Icon::Disk => {
            p.circle_stroke(at(8.0, 8.0), 6.0 * s, st);
            p.circle_stroke(at(8.0, 8.0), 1.6 * s, st);
        }
        Icon::Phone => {
            rr(4.5, 1.0, 11.5, 15.0, 1.8);
            line(at(7.0, 2.8), at(9.0, 2.8));
            p.circle_filled(at(8.0, 13.0), 0.8 * s, color);
        }
        Icon::Tablet => {
            rr(1.5, 2.5, 14.5, 13.5, 1.6);
            p.circle_filled(at(13.0, 8.0), 0.7 * s, color);
        }
        Icon::Volume | Icon::Muted => {
            p.add(Shape::convex_polygon(vec![at(1.5, 6.0), at(4.5, 6.0), at(8.5, 2.5), at(8.5, 13.5), at(4.5, 10.0), at(1.5, 10.0)], color, Stroke::NONE));
            if icon == Icon::Volume {
                poly(arc(at(8.5, 8.0), 3.2 * s, -0.8, 0.8));
                poly(arc(at(8.5, 8.0), 5.8 * s, -0.85, 0.85));
            } else {
                line(at(10.5, 5.5), at(15.0, 10.5));
                line(at(15.0, 5.5), at(10.5, 10.5));
            }
        }
        Icon::Keyboard => {
            rr(1.0, 4.0, 15.0, 12.0, 1.5);
            for x in [4.0, 6.5, 9.0, 11.5] {
                p.circle_filled(at(x, 6.8), 0.6 * s, color);
            }
            line(at(5.0, 9.5), at(11.0, 9.5));
        }
        Icon::Remote => {
            let c = at(8.0, 11.5);
            p.circle_filled(c, 1.4 * s, color);
            poly(arc(c, 4.2 * s, -2.4, -0.74));
            poly(arc(c, 7.2 * s, -2.45, -0.69));
        }
    }
}
