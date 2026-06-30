//! Mouse input: `Mouse`, `Button`, `Modifiers`, `Shape`, and `Type`.

/// A mouse event.
///
/// `col` and `row` are signed: the parser saturates negative pixel-to-cell
/// conversions rather than clamping to zero, so out-of-bounds positions remain
/// representable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mouse {
    pub col: i16,
    pub row: i16,
    pub xoffset: u16,
    pub yoffset: u16,
    pub button: Button,
    pub mods: Modifiers,
    /// The event type. Named `kind` because `type` is a reserved word in Rust.
    pub kind: Type,
}

/// The cursor shape requested for the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Shape {
    #[default]
    Default,
    Text,
    Pointer,
    Help,
    Progress,
    Wait,
    EwResize,
    NsResize,
    Cell,
}

impl Shape {
    /// Returns the CSS cursor-shape name carried by the OSC 22 set-cursor-shape
    /// sequence. Note the hyphenated `ew-resize` and `ns-resize`.
    pub fn to_wire(self) -> &'static str {
        match self {
            Shape::Default => "default",
            Shape::Text => "text",
            Shape::Pointer => "pointer",
            Shape::Help => "help",
            Shape::Progress => "progress",
            Shape::Wait => "wait",
            Shape::EwResize => "ew-resize",
            Shape::NsResize => "ns-resize",
            Shape::Cell => "cell",
        }
    }
}

/// A mouse button. Discriminants are the SGR wire codes and are sparse: the
/// wheel and extended buttons sit at fixed offsets defined by the protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Button {
    Left = 0,
    Middle = 1,
    Right = 2,
    None = 3,
    WheelUp = 64,
    WheelDown = 65,
    WheelRight = 66,
    WheelLeft = 67,
    Button8 = 128,
    Button9 = 129,
    Button10 = 130,
    Button11 = 131,
}

/// Error from converting a raw byte to a [`Button`]: the value is not a known
/// button code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("unknown mouse button code: {0}")]
pub struct InvalidButton(pub u8);

impl TryFrom<u8> for Button {
    type Error = InvalidButton;

    /// The parser masks the SGR button field with `0xC3`, which can still
    /// produce codes that map to no button, so we reject unmapped values
    /// rather than reaching for an undefined discriminant.
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Ok(match value {
            0 => Button::Left,
            1 => Button::Middle,
            2 => Button::Right,
            3 => Button::None,
            64 => Button::WheelUp,
            65 => Button::WheelDown,
            66 => Button::WheelRight,
            67 => Button::WheelLeft,
            128 => Button::Button8,
            129 => Button::Button9,
            130 => Button::Button10,
            131 => Button::Button11,
            other => return Err(InvalidButton(other)),
        })
    }
}

bitflags::bitflags! {
    /// Mouse modifier keys. Distinct from [`crate::key::Modifiers`]: SGR mouse
    /// reports encode only shift, alt, and ctrl, in that bit order.
    ///
    /// Upstream backs this with a `u3`. Rust has no `u3`, so we use the low
    /// three bits of a `u8`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
    pub struct Modifiers: u8 {
        const SHIFT = 1 << 0;
        const ALT = 1 << 1;
        const CTRL = 1 << 2;
    }
}

/// The kind of mouse event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Type {
    Press,
    Release,
    Motion,
    Drag,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn button_try_from_round_trips() {
        let cases = [
            (0u8, Button::Left),
            (1, Button::Middle),
            (2, Button::Right),
            (3, Button::None),
            (64, Button::WheelUp),
            (65, Button::WheelDown),
            (66, Button::WheelRight),
            (67, Button::WheelLeft),
            (128, Button::Button8),
            (129, Button::Button9),
            (130, Button::Button10),
            (131, Button::Button11),
        ];
        for (raw, button) in cases {
            assert_eq!(Button::try_from(raw), Ok(button));
        }
    }

    #[test]
    fn button_try_from_rejects_unmapped() {
        assert_eq!(Button::try_from(192), Err(InvalidButton(192)));
    }

    #[test]
    fn shape_wire_strings() {
        assert_eq!(Shape::EwResize.to_wire(), "ew-resize");
        assert_eq!(Shape::NsResize.to_wire(), "ns-resize");
    }
}
