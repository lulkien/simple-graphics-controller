//! App logic: the bouncing-rect animation state.
//!
//! The 16ms logic timer in `render::run` calls [`Motion::tick`] — this is
//! the logic half of the LVGL-style split (logic advances state, the 33ms
//! draw timer repaints it). Every wall bounce picks a new random color.
//!
//! Screen and rect sizes are passed in by the renderer (real framebuffer
//! resolution), not hardcoded here.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const LOGIC_INTERVAL: Duration = Duration::from_millis(16);

/// A rect bouncing off the screen edges at a constant velocity, changing
/// to a random color on every bounce. RGBA is a plain tuple so this module
/// stays free of linfb types; the renderer converts via `Color::from`.
pub struct Motion {
    screen_w: i32,
    screen_h: i32,
    pos_x: i32,
    pos_y: i32,
    dx: i32,
    dy: i32,
    rect_w: i32,
    rect_h: i32,
    color: (u8, u8, u8, u8),
    /// xorshift64* state — dependency-free PRNG for the bounce colors.
    rng: u64,
}

impl Motion {
    /// `screen_w`/`screen_h`: the framebuffer resolution. `rect_w`/`rect_h`:
    /// the bouncing rect's size.
    pub fn new(screen_w: i32, screen_h: i32, rect_w: i32, rect_h: i32) -> Self {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9e37_79b9_7f4a_7c15);

        Self {
            screen_w,
            screen_h,
            pos_x: 0,
            pos_y: 0,
            dx: 4,
            dy: 3,
            rect_w,
            rect_h,
            color: (0x00, 0xff, 0x00, 0x99),
            rng: seed,
        }
    }

    /// One xorshift64* step.
    fn next_random(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x
    }

    /// Pick a new random rect color: random RGB, same translucent alpha as
    /// the initial one. Stores and returns it.
    pub fn random_color(&mut self) -> (u8, u8, u8, u8) {
        let color = (
            (self.next_random() >> 24) as u8,
            (self.next_random() >> 24) as u8,
            (self.next_random() >> 24) as u8,
            0x99,
        );
        self.color = color;
        color
    }

    /// The current rect color (RGBA).
    pub fn color(&self) -> (u8, u8, u8, u8) {
        self.color
    }

    /// Advance one logic tick and bounce off the edges. On a wall hit, the
    /// rect picks a new random color.
    pub fn tick(&mut self) -> (i32, i32) {
        self.pos_x += self.dx;
        self.pos_y += self.dy;

        let mut bounced = false;
        if self.pos_x <= 0 {
            self.pos_x = 0;
            self.dx = self.dx.abs();
            bounced = true;
        } else if self.pos_x + self.rect_w >= self.screen_w {
            self.pos_x = self.screen_w - self.rect_w;
            self.dx = -self.dx.abs();
            bounced = true;
        }

        if self.pos_y <= 0 {
            self.pos_y = 0;
            self.dy = self.dy.abs();
            bounced = true;
        } else if self.pos_y + self.rect_h >= self.screen_h {
            self.pos_y = self.screen_h - self.rect_h;
            self.dy = -self.dy.abs();
            bounced = true;
        }

        if bounced {
            self.random_color();
        }

        (self.pos_x, self.pos_y)
    }

    pub fn position(&self) -> (i32, i32) {
        (self.pos_x, self.pos_y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounce_changes_color() {
        let mut motion = Motion::new(800, 600, 100, 100);
        let initial = motion.color();

        // 1000 ticks guarantees several wall hits (first bounce at ~167).
        for _ in 0..1000 {
            motion.tick();
        }

        assert_ne!(motion.color(), initial, "a bounce must change the color");
        assert_eq!(motion.color().3, 0x99, "alpha stays translucent");
    }

    #[test]
    fn random_color_varies() {
        let mut motion = Motion::new(800, 600, 100, 100);
        let first = motion.random_color();
        let mut all_same = true;
        for _ in 0..10 {
            if motion.random_color() != first {
                all_same = false;
            }
        }
        assert!(!all_same, "10 draws must not all collide");
    }

    #[test]
    fn rect_stays_within_screen_bounds() {
        let mut motion = Motion::new(800, 600, 100, 100);
        for _ in 0..10_000 {
            motion.tick();
        }
        let (x, y) = motion.position();
        assert!((0..=700).contains(&x), "x={x} out of bounds");
        assert!((0..=500).contains(&y), "y={y} out of bounds");
    }
}
