//! The calculator itself: a text field, a mode, and what every button and key
//! does to them.
//!
//! Nothing here draws. Every button is one [`Action`] and every key reaches the
//! same [`Action`], so what the mouse can do and what the keyboard can do are
//! one list rather than two that drift.

use crate::error::EvalError;
use crate::num::{Angle, Num};
use crate::parser;
use crate::prog::{self, Base};

/// Which layout is up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Calc,
    Prog,
}

/// What a button — or a key — does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    /// Type this at the caret.
    Insert(&'static str),
    Equals,
    Clear,
    Backspace,
    Delete,
    /// Flip the sign of the number the caret sits after.
    Negate,
    Left,
    Right,
    Home,
    End,
    SetMode(Mode),
    ToggleAngle,
    SetBase(Base),
}

/// One key of a layout: what it says and what it does.
pub struct Button {
    pub label: &'static str,
    pub action: Action,
}

const fn b(label: &'static str, action: Action) -> Button {
    Button { label, action }
}

/// The Calc layout, row-major across eight columns. The first three are the
/// scientific block and the last five the pad.
pub const CALC_BUTTONS: [Button; 32] = [
    b("sin", Action::Insert("sin(")),
    b("cos", Action::Insert("cos(")),
    b("tan", Action::Insert("tan(")),
    b("C", Action::Clear),
    b("\u{2190}", Action::Backspace),
    b("%", Action::Insert("%")),
    b("\u{00B1}", Action::Negate),
    b("\u{00F7}", Action::Insert("÷")),
    //
    b("ln", Action::Insert("ln(")),
    b("log", Action::Insert("log(")),
    b("x^y", Action::Insert("^")),
    b("7", Action::Insert("7")),
    b("8", Action::Insert("8")),
    b("9", Action::Insert("9")),
    b("0", Action::Insert("0")),
    b("\u{00D7}", Action::Insert("×")),
    //
    b("\u{03C0}", Action::Insert("π")),
    b("e", Action::Insert("e")),
    b("\u{221A}", Action::Insert("√")),
    b("4", Action::Insert("4")),
    b("5", Action::Insert("5")),
    b("6", Action::Insert("6")),
    b(".", Action::Insert(".")),
    b("\u{2212}", Action::Insert("−")),
    //
    b("RAD", Action::ToggleAngle),
    b("(", Action::Insert("(")),
    b(")", Action::Insert(")")),
    b("1", Action::Insert("1")),
    b("2", Action::Insert("2")),
    b("3", Action::Insert("3")),
    b("=", Action::Equals),
    b("+", Action::Insert("+")),
];

/// The Prog layout, in the same eight-column shape.
pub const PROG_BUTTONS: [Button; 32] = [
    b("HEX", Action::SetBase(Base::Hex)),
    b("DEC", Action::SetBase(Base::Dec)),
    b("BIN", Action::SetBase(Base::Bin)),
    b("A", Action::Insert("A")),
    b("B", Action::Insert("B")),
    b("7", Action::Insert("7")),
    b("8", Action::Insert("8")),
    b("9", Action::Insert("9")),
    //
    b("AND", Action::Insert("&")),
    b("OR", Action::Insert("|")),
    b("XOR", Action::Insert("^")),
    b("C", Action::Insert("C")),
    b("D", Action::Insert("D")),
    b("4", Action::Insert("4")),
    b("5", Action::Insert("5")),
    b("6", Action::Insert("6")),
    //
    b("NOT", Action::Insert("~")),
    b("<<", Action::Insert("<<")),
    b(">>", Action::Insert(">>")),
    b("E", Action::Insert("E")),
    b("F", Action::Insert("F")),
    b("1", Action::Insert("1")),
    b("2", Action::Insert("2")),
    b("3", Action::Insert("3")),
    //
    b("CLR", Action::Clear),
    b("\u{2190}", Action::Backspace),
    b("=", Action::Equals),
    b("+", Action::Insert("+")),
    b("\u{2212}", Action::Insert("−")),
    b("\u{00D7}", Action::Insert("×")),
    b("\u{00F7}", Action::Insert("÷")),
    b("0", Action::Insert("0")),
];

/// Whether this button can be pressed right now — a hexadecimal letter is
/// nothing but a wrong answer while the base is decimal.
pub fn enabled(button: &Button, mode: Mode, base: Base) -> bool {
    if mode != Mode::Prog {
        return true;
    }
    match button.action {
        Action::Insert(text) => {
            let mut chars = text.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) if c.is_ascii_alphanumeric() => c.to_digit(base.radix()).is_some(),
                _ => true,
            }
        }
        _ => true,
    }
}

pub struct Calc {
    mode: Mode,
    angle: Angle,
    base: Base,
    expr: String,
    /// A byte index into `expr`, always on a character boundary.
    caret: usize,
    /// The live value of the Calc expression, when it has one.
    result: Option<Num>,
    /// The value the Prog panes show.
    value: i64,
    message: Option<String>,
}

impl Default for Calc {
    fn default() -> Self {
        Calc::new()
    }
}

impl Calc {
    pub fn new() -> Calc {
        Calc {
            mode: Mode::Calc,
            angle: Angle::Rad,
            base: Base::Dec,
            expr: String::new(),
            caret: 0,
            result: None,
            value: 0,
            message: None,
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn angle(&self) -> Angle {
        self.angle
    }

    pub fn base(&self) -> Base {
        self.base
    }

    pub fn expr(&self) -> &str {
        &self.expr
    }

    pub fn caret(&self) -> usize {
        self.caret
    }

    pub fn value(&self) -> i64 {
        self.value
    }

    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    /// The result line: the live value of what is typed, or nothing while it is
    /// not yet an expression.
    pub fn preview(&self) -> Option<String> {
        self.result.as_ref().map(|n| n.display())
    }

    pub fn buttons(&self) -> &'static [Button; 32] {
        match self.mode {
            Mode::Calc => &CALC_BUTTONS,
            Mode::Prog => &PROG_BUTTONS,
        }
    }

    /// What the angle button says, which is also what it is.
    pub fn angle_label(&self) -> &'static str {
        match self.angle {
            Angle::Rad => "RAD",
            Angle::Deg => "DEG",
        }
    }

    pub fn act(&mut self, action: Action) {
        match action {
            Action::Insert(text) => self.insert(text),
            Action::Equals => self.equals(),
            Action::Clear => self.clear(),
            Action::Backspace => self.backspace(),
            Action::Delete => self.delete(),
            Action::Negate => self.negate(),
            Action::Left => self.caret = self.prev_boundary(),
            Action::Right => self.caret = self.next_boundary(),
            Action::Home => self.caret = 0,
            Action::End => self.caret = self.expr.len(),
            Action::SetMode(mode) => self.set_mode(mode),
            Action::ToggleAngle => {
                self.angle = match self.angle {
                    Angle::Rad => Angle::Deg,
                    Angle::Deg => Angle::Rad,
                };
                self.message = None;
                self.refresh();
            }
            Action::SetBase(base) => self.set_base(base),
        }
    }

    /// A typed character, refused where the mode has no use for it.
    pub fn type_char(&mut self, c: char) {
        if self.mode == Mode::Prog && c.is_ascii_alphanumeric() && c.to_digit(self.base.radix()).is_none() {
            return;
        }
        let mut buf = [0u8; 4];
        self.insert_owned(c.encode_utf8(&mut buf));
    }

    pub fn insert(&mut self, text: &str) {
        self.insert_owned(text);
    }

    fn insert_owned(&mut self, text: &str) {
        if self.expr.len() + text.len() > parser::MAX_LEN {
            self.message = Some(EvalError::TooLong.message());
            return;
        }
        self.expr.insert_str(self.caret, text);
        self.caret += text.len();
        self.message = None;
        self.refresh();
    }

    pub fn clear(&mut self) {
        self.expr.clear();
        self.caret = 0;
        self.result = None;
        self.message = None;
        if self.mode == Mode::Prog {
            self.value = 0;
        }
    }

    pub fn backspace(&mut self) {
        let at = self.prev_boundary();
        if at != self.caret {
            self.expr.replace_range(at..self.caret, "");
            self.caret = at;
            self.message = None;
            self.refresh();
        }
    }

    pub fn delete(&mut self) {
        let to = self.next_boundary();
        if to != self.caret {
            self.expr.replace_range(self.caret..to, "");
            self.message = None;
            self.refresh();
        }
    }

    pub fn equals(&mut self) {
        if self.expr.trim().is_empty() {
            self.message = None;
            return;
        }
        match self.mode {
            Mode::Calc => match parser::eval(&self.expr, self.angle) {
                Ok(value) => {
                    self.result = Some(value);
                    self.message = None;
                }
                Err(e) => self.message = Some(e.message()),
            },
            Mode::Prog => match prog::eval(&self.expr, self.base) {
                Ok(value) => {
                    self.value = value;
                    self.message = None;
                }
                Err(e) => self.message = Some(e.message()),
            },
        }
    }

    /// Flip the sign of the number the caret sits after, or start a negative
    /// one where there is no number to flip.
    fn negate(&mut self) {
        let head = &self.expr[..self.caret];
        let start = head
            .char_indices()
            .rev()
            .take_while(|(_, c)| c.is_ascii_digit() || *c == '.')
            .last()
            .map(|(i, _)| i);
        let Some(start) = start else {
            self.insert_owned("-");
            return;
        };
        // A leading `-` belongs to this number only when what precedes it
        // cannot end a value.
        let signed = start > 0 && self.expr.as_bytes()[start - 1] == b'-' && {
            let before = self.expr[..start - 1].trim_end();
            before.is_empty() || before.ends_with(['(', '+', '-', '*', '/', '^', '×', '÷', '−'])
        };
        if signed {
            self.expr.remove(start - 1);
            self.caret -= 1;
        } else {
            self.expr.insert(start, '-');
            self.caret += 1;
        }
        self.message = None;
        self.refresh();
    }

    fn set_mode(&mut self, mode: Mode) {
        if mode == self.mode {
            return;
        }
        match mode {
            Mode::Prog => {
                let carried = match &self.result {
                    None => Ok(None),
                    Some(Num::Exact(r)) if r.is_integer() => {
                        match r.to_int().expect("an integer rational").to_i64() {
                            Some(v) => Ok(Some(v)),
                            None => Err(EvalError::OutOfRange),
                        }
                    }
                    Some(_) => Err(EvalError::NotAnInteger),
                };
                self.mode = Mode::Prog;
                match carried {
                    Ok(Some(v)) => {
                        self.value = v;
                        self.expr = prog::write_in_base(v, self.base);
                        self.caret = self.expr.len();
                        self.message = None;
                    }
                    Ok(None) => {
                        self.value = 0;
                        self.expr.clear();
                        self.caret = 0;
                        self.message = None;
                    }
                    Err(why) => {
                        self.value = 0;
                        self.expr.clear();
                        self.caret = 0;
                        self.message = Some(format!("cleared — {}", why.message()));
                    }
                }
                self.result = None;
            }
            Mode::Calc => {
                self.mode = Mode::Calc;
                if !(self.expr.is_empty() && self.value == 0) {
                    self.expr = self.value.to_string();
                    self.caret = self.expr.len();
                }
                self.message = None;
                self.refresh();
            }
        }
    }

    /// Changing the base changes what every typed digit meant, so the entry is
    /// written again from the value rather than reinterpreted. An entry with
    /// nothing in it has nothing to write.
    fn set_base(&mut self, base: Base) {
        if base == self.base {
            return;
        }
        self.base = base;
        if !self.expr.is_empty() {
            self.expr = prog::write_in_base(self.value, base);
            self.caret = self.expr.len();
        }
        self.message = None;
    }

    /// Work the expression out again, quietly: a half-typed expression is not
    /// an error anyone needs to be told about.
    fn refresh(&mut self) {
        match self.mode {
            Mode::Calc => self.result = parser::eval(&self.expr, self.angle).ok(),
            Mode::Prog => {
                if let Ok(v) = prog::eval(&self.expr, self.base) {
                    self.value = v;
                }
            }
        }
    }

    fn prev_boundary(&self) -> usize {
        self.expr[..self.caret].char_indices().next_back().map(|(i, _)| i).unwrap_or(0)
    }

    fn next_boundary(&self) -> usize {
        match self.expr[self.caret..].chars().next() {
            Some(c) => self.caret + c.len_utf8(),
            None => self.caret,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(text: &str) -> Calc {
        let mut c = Calc::new();
        for ch in text.chars() {
            c.type_char(ch);
        }
        c
    }

    /// Press the button with this label in the layout that is up.
    fn press(c: &mut Calc, label: &str) {
        let button = c
            .buttons()
            .iter()
            .find(|b| b.label == label)
            .unwrap_or_else(|| panic!("no button says {label}"));
        c.act(button.action);
    }

    #[test]
    fn the_expression_line_is_a_text_field() {
        let mut c = typed("12+3");
        assert_eq!(c.expr(), "12+3");
        assert_eq!(c.caret(), 4);
        c.act(Action::Left);
        c.act(Action::Left);
        assert_eq!(c.caret(), 2);
        c.type_char('4');
        assert_eq!(c.expr(), "124+3");
        c.act(Action::Home);
        assert_eq!(c.caret(), 0);
        c.act(Action::Delete);
        assert_eq!(c.expr(), "24+3");
        c.act(Action::End);
        c.act(Action::Backspace);
        assert_eq!(c.expr(), "24+");
        c.act(Action::Clear);
        assert_eq!(c.expr(), "");
        assert_eq!(c.preview(), None);
    }

    #[test]
    fn a_caret_in_the_middle_of_a_wide_character_is_unreachable() {
        let mut c = typed("π+e");
        c.act(Action::Left);
        c.act(Action::Left);
        c.act(Action::Left);
        assert_eq!(c.caret(), 0);
        c.act(Action::Right);
        assert_eq!(c.caret(), "π".len());
        c.act(Action::End);
        c.act(Action::Backspace);
        c.act(Action::Backspace);
        assert_eq!(c.expr(), "π");
    }

    #[test]
    fn typing_previews_and_equals_commits() {
        let mut c = typed("1÷3×3");
        assert_eq!(c.preview(), Some("1".to_string()));
        c.act(Action::Equals);
        assert_eq!(c.message(), None);
        assert_eq!(c.preview(), Some("1".to_string()));

        let mut c = typed("1÷0");
        assert_eq!(c.preview(), None);
        c.act(Action::Equals);
        assert_eq!(c.message(), Some("division by zero"));
    }

    #[test]
    fn every_button_reaches_the_same_place_a_key_does() {
        let mut c = Calc::new();
        for label in ["1", "2", "\u{00F7}", "3"] {
            press(&mut c, label);
        }
        assert_eq!(c.expr(), "12÷3");
        assert_eq!(c.preview(), Some("4".to_string()));
        press(&mut c, "\u{2190}");
        assert_eq!(c.expr(), "12÷");
        press(&mut c, "C");
        assert_eq!(c.expr(), "");

        press(&mut c, "sin");
        assert_eq!(c.expr(), "sin(");
        press(&mut c, "0");
        press(&mut c, ")");
        assert_eq!(c.preview(), Some("≈0".to_string()));
    }

    #[test]
    fn the_angle_button_says_which_one_it_is() {
        let mut c = typed("sin(90)");
        assert_eq!(c.angle_label(), "RAD");
        press(&mut c, "RAD");
        assert_eq!(c.angle_label(), "DEG");
        assert_eq!(c.preview(), Some("≈1".to_string()));
        c.act(Action::ToggleAngle);
        assert_eq!(c.angle_label(), "RAD");
    }

    #[test]
    fn the_sign_button_flips_the_number_it_stands_after() {
        let mut c = typed("12");
        c.act(Action::Negate);
        assert_eq!(c.expr(), "-12");
        c.act(Action::Negate);
        assert_eq!(c.expr(), "12");

        let mut c = typed("3+4");
        c.act(Action::Negate);
        assert_eq!(c.expr(), "3+-4");
        assert_eq!(c.preview(), Some("-1".to_string()));
        c.act(Action::Negate);
        assert_eq!(c.expr(), "3+4");

        let mut c = Calc::new();
        c.act(Action::Negate);
        assert_eq!(c.expr(), "-");
    }

    #[test]
    fn a_whole_number_crosses_into_prog_and_back() {
        let mut c = typed("255");
        c.act(Action::SetMode(Mode::Prog));
        assert_eq!(c.mode(), Mode::Prog);
        assert_eq!(c.value(), 255);
        assert_eq!(c.expr(), "255");
        assert_eq!(c.message(), None);

        press(&mut c, "HEX");
        assert_eq!(c.base(), Base::Hex);
        assert_eq!(c.expr(), "FF");
        assert_eq!(c.value(), 255);

        c.act(Action::SetMode(Mode::Calc));
        assert_eq!(c.expr(), "255");
        assert_eq!(c.preview(), Some("255".to_string()));
    }

    #[test]
    fn a_fraction_is_cleared_on_the_way_into_prog_rather_than_rounded() {
        let mut c = typed("1÷3");
        c.act(Action::SetMode(Mode::Prog));
        assert_eq!(c.expr(), "");
        assert_eq!(c.value(), 0);
        assert_eq!(c.message(), Some("cleared — not a whole number"));

        let mut c = typed("√2");
        c.act(Action::SetMode(Mode::Prog));
        assert_eq!(c.message(), Some("cleared — not a whole number"));

        let mut c = typed("2^100");
        c.act(Action::SetMode(Mode::Prog));
        assert_eq!(c.message(), Some("cleared — outside 64 bits"));

        let mut c = Calc::new();
        c.act(Action::SetMode(Mode::Prog));
        assert_eq!(c.message(), None);
    }

    #[test]
    fn prog_reads_digits_in_the_active_base_and_refuses_the_others() {
        let mut c = Calc::new();
        c.act(Action::SetMode(Mode::Prog));
        c.act(Action::SetBase(Base::Hex));
        for ch in "FF".chars() {
            c.type_char(ch);
        }
        assert_eq!(c.expr(), "FF");
        assert_eq!(c.value(), 255);

        c.act(Action::Clear);
        c.act(Action::SetBase(Base::Dec));
        for ch in "1F2".chars() {
            c.type_char(ch);
        }
        assert_eq!(c.expr(), "12", "F is not a decimal digit");
        assert_eq!(c.value(), 12);

        c.act(Action::Clear);
        for ch in "7÷2".chars() {
            c.type_char(ch);
        }
        c.act(Action::Equals);
        assert_eq!(c.message(), Some("not a whole number"));
    }

    #[test]
    fn the_hex_letters_are_live_only_in_hex() {
        let a = PROG_BUTTONS.iter().find(|b| b.label == "A").unwrap();
        let seven = PROG_BUTTONS.iter().find(|b| b.label == "7").unwrap();
        assert!(enabled(a, Mode::Prog, Base::Hex));
        assert!(!enabled(a, Mode::Prog, Base::Dec));
        assert!(!enabled(a, Mode::Prog, Base::Bin));
        assert!(enabled(seven, Mode::Prog, Base::Dec));
        assert!(!enabled(seven, Mode::Prog, Base::Bin));
        let and = PROG_BUTTONS.iter().find(|b| b.label == "AND").unwrap();
        assert!(enabled(and, Mode::Prog, Base::Bin));
        assert!(enabled(a, Mode::Calc, Base::Bin));
    }

    #[test]
    fn a_field_that_is_full_says_so_rather_than_growing() {
        let mut c = Calc::new();
        for _ in 0..parser::MAX_LEN {
            c.type_char('1');
        }
        assert_eq!(c.expr().len(), parser::MAX_LEN);
        c.type_char('1');
        assert_eq!(c.expr().len(), parser::MAX_LEN);
        assert_eq!(c.message(), Some("expression too long"));
    }

    #[test]
    fn every_button_of_both_layouts_is_pressable_from_any_state() {
        for layout in [&CALC_BUTTONS, &PROG_BUTTONS] {
            for button in layout.iter() {
                let mut c = typed("1+2×(3");
                c.act(button.action);
                c.act(Action::Equals);
                let mut c = Calc::new();
                c.act(Action::SetMode(Mode::Prog));
                c.act(button.action);
                c.act(Action::Equals);
            }
        }
    }
}
