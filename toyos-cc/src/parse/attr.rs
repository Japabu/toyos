use super::Parser;
use crate::lex::TokenKind;

/// What a GNU attribute list changed about the type it was attached to.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TypeAttrs {
    pub packed: bool,
    pub align: Option<usize>,
}

impl TypeAttrs {
    pub fn merge(self, other: Self) -> Self {
        Self {
            packed: self.packed || other.packed,
            align: match (self.align, other.align) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            },
        }
    }

    pub fn is_empty(self) -> bool {
        self == Self::default()
    }
}

/// Where an attribute list was found. Only a struct or union definition has
/// somewhere to put a layout change; the rest refuse one by name.
#[derive(Clone, Copy)]
pub enum AttrSite {
    StructOrUnion,
    /// The position, as it should read in a refusal.
    Elsewhere(&'static str),
}

/// Attributes toyos-cc accepts and then does nothing about, each with the
/// reason doing nothing is the same as obeying it. An attribute that cannot be
/// given one belongs in neither this list nor the compiler.
fn no_effect_reason(name: &str) -> Option<&'static str> {
    match name {
        "unused" | "maybe_unused" => {
            Some("suppresses an unused-entity diagnostic and toyos-cc emits none")
        }
        "noinline" => Some(
            "forbids inlining, and every C function is a separate Cranelift \
             function that nothing inlines across",
        ),
        "noreturn" => Some(
            "promises control never comes back, which buys a caller an \
             unreachable epilogue and no change in behaviour",
        ),
        "format" => Some(
            "asks for a printf-style format string to be checked against its \
             arguments, and toyos-cc emits no diagnostics to check it with",
        ),
        "stdcall" | "fastcall" | "cdecl" => Some(
            "names an x86-32 calling convention; x86-64 and aarch64 have only \
             one, and gcc and clang ignore it on both",
        ),
        _ => None,
    }
}

impl Parser {
    /// Attributes on a struct or union definition, where `packed` and
    /// `aligned` are applied.
    pub(super) fn type_attributes(&mut self) -> TypeAttrs {
        self.attributes(AttrSite::StructOrUnion)
    }

    /// Attributes in a position toyos-cc has nowhere to put a layout change.
    pub(super) fn discard_attributes(&mut self, site: &'static str) {
        self.attributes(AttrSite::Elsewhere(site));
    }

    fn attributes(&mut self, site: AttrSite) -> TypeAttrs {
        let mut attrs = TypeAttrs::default();
        while matches!(self.peek(), TokenKind::Attribute) {
            self.advance();
            self.expect(&TokenKind::LParen);
            self.expect(&TokenKind::LParen);
            while self.peek() != &TokenKind::RParen {
                if self.eat(&TokenKind::Comma) {
                    continue;
                }
                attrs = attrs.merge(self.one_attribute(site));
            }
            self.expect(&TokenKind::RParen);
            self.expect(&TokenKind::RParen);
        }
        attrs
    }

    fn one_attribute(&mut self, site: AttrSite) -> TypeAttrs {
        let loc = self.loc();
        let spelling = match self.peek().clone() {
            TokenKind::Ident(s) => {
                self.advance();
                s
            }
            other => panic!("{loc}: expected an attribute name, got {other}"),
        };
        // gcc spells every attribute both plainly and wrapped in underscores.
        let name = spelling
            .strip_prefix("__")
            .and_then(|s| s.strip_suffix("__"))
            .unwrap_or(&spelling)
            .to_string();

        let layout = match name.as_str() {
            "packed" => TypeAttrs { packed: true, align: None },
            "aligned" => TypeAttrs { packed: false, align: Some(self.aligned_argument(&loc)) },
            _ => {
                if self.peek() == &TokenKind::LParen {
                    self.skip_balanced_parens();
                }
                match no_effect_reason(&name) {
                    Some(why) => {
                        verbose!("{loc}: attribute '{spelling}' has no effect here: {why}");
                        return TypeAttrs::default();
                    }
                    None => panic!(
                        "{loc}: __attribute__(({spelling})) is not implemented by toyos-cc. \
                         Attributes it implements: packed, aligned. Attributes it accepts \
                         and ignores: unused, maybe_unused, noinline, noreturn, format, \
                         stdcall, fastcall, cdecl."
                    ),
                }
            }
        };

        match site {
            AttrSite::StructOrUnion => layout,
            AttrSite::Elsewhere(where_) => panic!(
                "{loc}: __attribute__(({spelling})) changes layout, and toyos-cc applies \
                 that only to a struct or union definition, not to {where_}"
            ),
        }
    }

    fn aligned_argument(&mut self, loc: &str) -> usize {
        assert!(
            self.peek() == &TokenKind::LParen,
            "{loc}: __attribute__((aligned)) without an alignment is not implemented by toyos-cc"
        );
        self.advance();
        let expr = self.conditional_expr();
        self.expect(&TokenKind::RParen);
        let n = crate::ast::eval_const_expr(&expr, Some(&self.type_env.enum_constants))
            .unwrap_or_else(|| panic!("{loc}: __attribute__((aligned(...))) needs a constant"));
        assert!(
            n > 0 && (n as u64).is_power_of_two(),
            "{loc}: __attribute__((aligned({n}))) is not a positive power of two"
        );
        n as usize
    }
}
