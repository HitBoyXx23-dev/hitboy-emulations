//! Console mode: the navigation state of the power-on sequence and the couch-friendly home
//! screen. Pure logic (no drawing) so it can be tested; `app/shell_ui.rs` draws it.

use crate::library::{Console, Status, CONSOLES};

/// How long the power-on animation plays unless the player skips it.
pub const BOOT_SECS: f64 = 3.2;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Boot,
    Home,
}

/// Which strip the arrow keys move along.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Row {
    Consoles,
    Games,
}

pub struct Shell {
    pub phase: Phase,
    pub row: Row,
    pub console: usize,
    pub game: usize,
    started: f64,
    /// Powered on but the clock hasn't been read yet (the app was still starting up).
    unstarted: bool,
}

/// The consoles shown on the home screen: the ones that can run something today.
pub fn consoles() -> Vec<&'static Console> {
    CONSOLES.iter().filter(|c| matches!(c.status, Status::Playable | Status::Experimental)).collect()
}

impl Shell {
    pub fn new() -> Shell {
        Shell { phase: Phase::Home, row: Row::Consoles, console: 0, game: 0, started: 0.0, unstarted: false }
    }

    /// Already powering on; the animation's clock starts on the first frame drawn.
    pub fn booting() -> Shell {
        Shell { phase: Phase::Boot, unstarted: true, ..Shell::new() }
    }

    /// Powers the console on: plays the boot sequence, then lands on the home screen.
    pub fn restart(&mut self, now: f64) {
        self.phase = Phase::Boot;
        self.row = Row::Consoles;
        self.game = 0;
        self.started = now;
        self.unstarted = false;
    }

    pub fn progress(&self, now: f64) -> f32 {
        ((now - self.started) / BOOT_SECS).clamp(0.0, 1.0) as f32
    }

    pub fn tick(&mut self, now: f64) {
        if self.unstarted {
            self.started = now;
            self.unstarted = false;
        }
        if self.phase == Phase::Boot && self.progress(now) >= 1.0 {
            self.phase = Phase::Home;
        }
    }

    pub fn skip(&mut self) {
        self.phase = Phase::Home;
    }

    /// Left (-1) or right (+1) along the focused strip. Consoles wrap around; games stop at the ends.
    pub fn step(&mut self, dir: i32, consoles: usize, games: usize) {
        match self.row {
            Row::Consoles if consoles > 0 => {
                self.console = (self.console as i32 + dir).rem_euclid(consoles as i32) as usize;
                self.game = 0;
            }
            Row::Games if games > 0 => self.game = (self.game as i32 + dir).clamp(0, games as i32 - 1) as usize,
            _ => {}
        }
    }

    /// Moves focus from the consoles down to the games, if there are any.
    pub fn down(&mut self, games: usize) {
        if self.row == Row::Consoles && games > 0 {
            self.row = Row::Games;
            self.game = 0;
        }
    }

    /// Moves focus back up to the consoles. False if it was already there.
    pub fn up(&mut self) -> bool {
        if self.row == Row::Games {
            self.row = Row::Consoles;
            true
        } else {
            false
        }
    }
}

impl Default for Shell {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_plays_then_lands_on_home_and_can_be_skipped() {
        let mut s = Shell::new();
        s.restart(10.0);
        assert_eq!(s.phase, Phase::Boot);
        assert_eq!(s.progress(10.0), 0.0);
        s.tick(11.0);
        assert_eq!(s.phase, Phase::Boot);
        assert!(s.progress(11.6) > 0.49 && s.progress(11.6) < 0.51);
        s.tick(10.0 + BOOT_SECS + 0.01);
        assert_eq!(s.phase, Phase::Home);
        s.restart(50.0);
        s.skip();
        assert_eq!(s.phase, Phase::Home);
    }

    #[test]
    fn a_slow_start_up_does_not_eat_the_animation() {
        let mut s = Shell::booting();
        s.tick(7.5); // first frame drawn 7.5 s after the app launched
        assert_eq!(s.phase, Phase::Boot);
        assert_eq!(s.progress(7.5), 0.0);
        s.tick(7.5 + BOOT_SECS + 0.1);
        assert_eq!(s.phase, Phase::Home);
    }

    #[test]
    fn consoles_wrap_and_games_clamp() {
        let mut s = Shell::new();
        s.step(-1, 5, 0);
        assert_eq!(s.console, 4, "left of the first console wraps to the last");
        s.step(1, 5, 0);
        assert_eq!(s.console, 0);
        s.down(0);
        assert_eq!(s.row, Row::Consoles, "nothing to go down to when there are no games");
        s.down(3);
        assert_eq!((s.row, s.game), (Row::Games, 0));
        s.step(-1, 5, 3);
        assert_eq!(s.game, 0);
        s.step(1, 5, 3);
        s.step(1, 5, 3);
        s.step(1, 5, 3);
        assert_eq!(s.game, 2, "stops at the last game");
        assert!(s.up());
        assert!(!s.up());
        s.step(1, 5, 3);
        assert_eq!((s.console, s.game), (1, 0), "changing console resets the game selection");
    }

    #[test]
    fn only_consoles_that_can_run_something_are_listed() {
        let list = consoles();
        assert!(list.iter().any(|c| c.id == "nes"));
        assert!(list.iter().any(|c| c.id == "ps4"));
        assert!(list.iter().all(|c| c.status != Status::Planned));
    }
}
