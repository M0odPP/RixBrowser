use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TabId(pub u64);

impl TabId {
    pub fn new(id: u64) -> Self {
        Self(id)
    }
}

impl fmt::Display for TabId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "tab-{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct WindowId(pub u64);

impl WindowId {
    pub fn new(id: u64) -> Self {
        Self(id)
    }
}

impl fmt::Display for WindowId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "win-{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ProcessId(pub u32);

impl ProcessId {
    pub fn new(id: u32) -> Self {
        Self(id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub const ZERO: Self = Self { x: 0.0, y: 0.0 };

    pub fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct Vec2 {
    pub x: f32,
    pub y: f32,
}

impl Vec2 {
    pub const ZERO: Self = Self { x: 0.0, y: 0.0 };

    pub fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct Size {
    pub width: f32,
    pub height: f32,
}

impl Size {
    pub const ZERO: Self = Self {
        width: 0.0,
        height: 0.0,
    };

    pub fn new(width: f32, height: f32) -> Self {
        Self { width, height }
    }

    pub fn is_empty(&self) -> bool {
        self.width <= 0.0 || self.height <= 0.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct Rect {
    pub origin: Point,
    pub size: Size,
}

impl Rect {
    pub const ZERO: Self = Self {
        origin: Point::ZERO,
        size: Size::ZERO,
    };

    pub fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            origin: Point::new(x, y),
            size: Size::new(width, height),
        }
    }

    #[inline]
    pub fn min_x(&self) -> f32 {
        self.origin.x
    }

    #[inline]
    pub fn max_x(&self) -> f32 {
        self.origin.x + self.size.width
    }

    #[inline]
    pub fn min_y(&self) -> f32 {
        self.origin.y
    }

    #[inline]
    pub fn max_y(&self) -> f32 {
        self.origin.y + self.size.height
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        !self.origin.x.is_finite()
            || !self.origin.y.is_finite()
            || !self.size.width.is_finite()
            || !self.size.height.is_finite()
            || self.size.width <= 0.0
            || self.size.height <= 0.0
    }

    #[inline]
    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.origin.x
            && p.x <= self.origin.x + self.size.width
            && p.y >= self.origin.y
            && p.y <= self.origin.y + self.size.height
    }

    #[inline]
    pub fn intersects(&self, other: &Rect) -> bool {
        !self.is_empty()
            && !other.is_empty()
            && self.origin.x < other.origin.x + other.size.width
            && self.origin.x + self.size.width > other.origin.x
            && self.origin.y < other.origin.y + other.size.height
            && self.origin.y + self.size.height > other.origin.y
    }

    #[inline]
    pub fn inflate(&self, margin: f32) -> Rect {
        Rect::new(
            self.origin.x - margin,
            self.origin.y - margin,
            (self.size.width + margin * 2.0).max(0.0),
            (self.size.height + margin * 2.0).max(0.0),
        )
    }

    #[inline]
    pub fn intersect(&self, other: &Rect) -> Option<Rect> {
        if self.is_empty() || other.is_empty() {
            return None;
        }
        let min_x = self.min_x().max(other.min_x());
        let min_y = self.min_y().max(other.min_y());
        let max_x = self.max_x().min(other.max_x());
        let max_y = self.max_y().min(other.max_y());
        let w = max_x - min_x;
        let h = max_y - min_y;
        if w > 0.0 && h > 0.0 && w.is_finite() && h.is_finite() {
            Some(Rect::new(min_x, min_y, w, h))
        } else {
            None
        }
    }

    #[inline]
    pub fn translate(&self, delta: Vec2) -> Rect {
        Rect::new(
            self.origin.x + delta.x,
            self.origin.y + delta.y,
            self.size.width,
            self.size.height,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NavigationState {
    Initial,
    Loading,
    DomInteractive,
    Complete,
    Failed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NavigationTiming {
    pub time_origin_ms: f64,
    pub fetch_start_ms: f64,
    pub request_start_ms: f64,
    pub response_start_ms: f64,
    pub response_end_ms: f64,
    pub redirect_start_ms: Option<f64>,
    pub redirect_end_ms: Option<f64>,
    pub redirect_count: u32,
}
