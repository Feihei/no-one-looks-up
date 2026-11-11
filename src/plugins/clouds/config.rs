//! Quality mapping and per-kind parameter tables (pure data, CPU-testable).

use bevy_elements::ElementsQuality;

/// Cloud rendering cost per [`ElementsQuality`] (R6; adr-0007). The march is
/// always half resolution per axis; the quality changes the step count and the
/// light-sample count only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloudsQuality {
    Low,
    Medium,
    High,
    Ultra,
}

impl CloudsQuality {
    /// The settings of a quality level.
    pub fn of(quality: ElementsQuality) -> Self {
        match quality {
            ElementsQuality::Low => Self::Low,
            ElementsQuality::Medium => Self::Medium,
            ElementsQuality::High => Self::High,
            ElementsQuality::Ultra => Self::Ultra,
        }
    }

    /// Primary ray-march steps (every texel is marched every frame, so these
    /// are per-texel budgets, not amortised ones).
    pub fn steps(self) -> u32 {
        match self {
            Self::Low => 40,
            Self::Medium => 56,
            Self::High => 72,
            Self::Ultra => 96,
        }
    }

    /// Samples along the path to the light.
    pub fn light_samples(self) -> u32 {
        match self {
            Self::Low => 4,
            Self::Medium => 6,
            Self::High => 6,
            Self::Ultra => 8,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_orders_step_counts() {
        assert!(
            CloudsQuality::of(ElementsQuality::Low).steps()
                <= CloudsQuality::of(ElementsQuality::Medium).steps()
        );
        assert!(
            CloudsQuality::of(ElementsQuality::Medium).steps()
                <= CloudsQuality::of(ElementsQuality::High).steps()
        );
        assert!(
            CloudsQuality::of(ElementsQuality::High).steps()
                <= CloudsQuality::of(ElementsQuality::Ultra).steps()
        );
    }
}
