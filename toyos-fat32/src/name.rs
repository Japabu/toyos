use alloc::string::String;

use crate::error::Error;

/// UTF-16 units a long file name may hold. Fixed by the format: the ordinal
/// field in a long-name entry is six bits, entries carry 13 units each, and
/// the specification stops at 20 entries.
pub const MAX_LFN_CHARS: usize = 255;

/// Long-name entries one name may occupy. `ceil(255 / 13)`.
pub const MAX_LFN_ENTRIES: usize = 20;

pub const UNITS_PER_LFN_ENTRY: usize = 13;

/// The raw 8.3 field: eight name bytes then three extension bytes, space
/// padded, no separator.
pub type ShortName = [u8; 11];

/// Set when the base or the extension of a short name was stored lowercase.
///
/// A Windows NT extension, not part of the FAT specification, and this crate
/// reads it without ever writing it. Reading is not optional — macOS and Linux
/// both set these bits, so a driver that ignores them reports `HELLO.TXT` for
/// a file every other system calls `hello.txt`. Writing them is: emitting a
/// long-name entry instead costs 32 bytes and depends on nothing outside the
/// specification.
pub const NT_LOWER_BASE: u8 = 0x08;
pub const NT_LOWER_EXT: u8 = 0x10;

/// Bytes a short name may not contain, beyond the control range.
const SHORT_ILLEGAL: &[u8] = b"\"*+,./:;<=>?[\\]|";

/// Characters a long name may not contain. `/` is absent because the caller
/// splits on it before a component reaches here.
const LFN_ILLEGAL: &[u8] = b"\"*/:<>?\\|";

fn short_byte_ok(b: u8) -> bool {
    b >= 0x20 && !SHORT_ILLEGAL.contains(&b) && b != b' '
}

/// Whether one path component is a name FAT can store.
///
/// Rejects rather than sanitises. A caller that asked for `a<b.txt` and got
/// `a_b.txt` has a file it cannot find by the name it chose, and will create a
/// second one next time.
pub fn validate_component(name: &str) -> Result<(), Error> {
    if name.is_empty() || name == "." || name == ".." {
        return Err(Error::InvalidName);
    }
    if name.ends_with(' ') || name.ends_with('.') || name.starts_with(' ') {
        return Err(Error::InvalidName);
    }
    let mut units = 0usize;
    for c in name.chars() {
        if (c as u32) < 0x20 {
            return Err(Error::InvalidName);
        }
        if c.is_ascii() && LFN_ILLEGAL.contains(&(c as u8)) {
            return Err(Error::InvalidName);
        }
        units += c.len_utf16();
        if units > MAX_LFN_CHARS {
            return Err(Error::InvalidName);
        }
    }
    Ok(())
}

/// Uppercase for name comparison.
///
/// ASCII only. Proper Unicode case folding needs tables this crate will not
/// carry, and FAT's own answer is an OEM-codepage table that differs by
/// volume — so two names that differ only outside ASCII compare as distinct
/// here, which is the conservative direction: it can refuse to find a file,
/// never confuse two.
fn fold(unit: u16) -> u16 {
    if (0x61..=0x7A).contains(&unit) {
        unit - 32
    } else {
        unit
    }
}

/// Case-insensitive comparison of a stored long name against a query.
pub fn long_name_eq(stored: &[u16], query: &str) -> bool {
    let mut q = query.encode_utf16();
    for &s in stored {
        match q.next() {
            Some(c) if fold(c) == fold(s) => {}
            _ => return false,
        }
    }
    q.next().is_none()
}

/// Render an 8.3 field as a name, honouring the NT case bits.
///
/// Total on any 11 bytes: the `0x05` first byte is the format's escape for a
/// leading `0xE5`, and every other byte is passed through as Latin-1, which is
/// lossy for a volume using a different OEM codepage but never fails.
pub fn short_name_to_string(short: &ShortName, nt_flags: u8) -> String {
    let mut out = String::with_capacity(12);
    let lower_base = nt_flags & NT_LOWER_BASE != 0;
    let lower_ext = nt_flags & NT_LOWER_EXT != 0;

    for (i, &b) in short[..8].iter().enumerate() {
        if b == b' ' {
            break;
        }
        let b = if i == 0 && b == 0x05 { 0xE5 } else { b };
        push_oem(&mut out, b, lower_base);
    }
    if short[8] != b' ' {
        out.push('.');
        for &b in &short[8..11] {
            if b == b' ' {
                break;
            }
            push_oem(&mut out, b, lower_ext);
        }
    }
    out
}

fn push_oem(out: &mut String, b: u8, lower: bool) {
    let c = b as char;
    out.push(if lower { c.to_ascii_lowercase() } else { c });
}

/// Case-insensitive comparison of an 8.3 field against a query.
///
/// The NT case bits deliberately play no part: they describe how to *display*
/// the name, and two files differing only in them would collide anyway.
pub fn short_name_eq(short: &ShortName, query: &str) -> bool {
    let rendered = short_name_to_string(short, 0);
    if rendered.len() != query.len() {
        return false;
    }
    rendered.bytes().zip(query.bytes()).all(|(a, b)| a.eq_ignore_ascii_case(&b))
}

/// The checksum a long-name entry carries so it can be tied to its short one.
///
/// Every long-name entry of a run repeats it. A mismatch means the run belongs
/// to a name that was deleted and partly overwritten, which is why this is the
/// check that decides whether to trust a reassembled name.
pub fn lfn_checksum(short: &ShortName) -> u8 {
    let mut sum: u8 = 0;
    for &b in short {
        sum = (sum >> 1) | (sum << 7);
        sum = sum.wrapping_add(b);
    }
    sum
}

/// The 8.3 name a long name reduces to before any uniquifying tail, and
/// whether that reduction lost anything.
pub struct Basis {
    pub short: ShortName,
    /// True when the short name is not the long name: characters were
    /// replaced, dropped, truncated, or uppercased.
    pub lossy: bool,
}

/// Reduce a long name to an 8.3 basis.
///
/// The algorithm is the specification's: strip leading periods and spaces,
/// take the text before the last period as the base and after it as the
/// extension, uppercase, replace what 8.3 cannot hold with `_`, and truncate.
pub fn basis_name(long: &str) -> Basis {
    let trimmed = long.trim_start_matches(['.', ' ']);
    let mut lossy = trimmed.len() != long.len();

    let (base_src, ext_src) = match trimmed.rfind('.') {
        Some(dot) => (&trimmed[..dot], &trimmed[dot + 1..]),
        None => (trimmed, ""),
    };

    let mut short = [b' '; 11];
    let mut n = 0;
    for c in base_src.chars() {
        let b = reduce_char(c, &mut lossy);
        let Some(b) = b else { continue };
        if n == 8 {
            lossy = true;
            break;
        }
        short[n] = b;
        n += 1;
    }
    // A name that reduces to nothing still needs an entry someone can find.
    if n == 0 {
        short[0] = b'_';
        lossy = true;
    }

    let mut n = 8;
    for c in ext_src.chars() {
        let b = reduce_char(c, &mut lossy);
        let Some(b) = b else { continue };
        if n == 11 {
            lossy = true;
            break;
        }
        short[n] = b;
        n += 1;
    }
    if short[0] == 0xE5 {
        short[0] = 0x05;
    }
    Basis { short, lossy }
}

fn reduce_char(c: char, lossy: &mut bool) -> Option<u8> {
    if c == ' ' || c == '.' {
        *lossy = true;
        return None;
    }
    let upper = c.to_ascii_uppercase();
    if upper != c {
        *lossy = true;
    }
    if !c.is_ascii() {
        *lossy = true;
        return Some(b'_');
    }
    let b = upper as u8;
    if short_byte_ok(b) {
        Some(b)
    } else {
        *lossy = true;
        Some(b'_')
    }
}

/// Whether a long name is exactly what its 8.3 basis renders back to, so no
/// long-name entries are needed.
pub fn fits_short(long: &str, basis: &Basis) -> bool {
    !basis.lossy && short_name_to_string(&basis.short, 0) == long
}

/// The `n`th candidate short name for a basis.
///
/// `n = 0` is the basis itself. `1..=4` append `~1`..`~4`, the specification's
/// numeric tail. Beyond that the tail carries a hash of the long name, because
/// the numeric tail alone degrades exactly where this crate is aimed: a
/// directory of `boot-0001.log`, `boot-0002.log`, … has one basis for every
/// file, so the `k`th one would need `~k`, and finding it means `k` directory
/// scans.
pub fn candidate(basis: &Basis, long: &str, n: u32) -> ShortName {
    if n == 0 {
        return basis.short;
    }
    let mut out = [b' '; 11];
    out[8..].copy_from_slice(&basis.short[8..]);

    if n <= 4 {
        let digit = b'0' + n as u8;
        let keep = short_len(&basis.short[..8]).min(6);
        out[..keep].copy_from_slice(&basis.short[..keep]);
        out[keep] = b'~';
        out[keep + 1] = digit;
        return out;
    }

    let h = fnv16(long).wrapping_add((n - 5) as u16);
    let keep = short_len(&basis.short[..8]).min(2);
    out[..keep].copy_from_slice(&basis.short[..keep]);
    for i in 0..4 {
        out[keep + i] = hex_digit((h >> (12 - 4 * i)) as u8 & 0x0F);
    }
    out[keep + 4] = b'~';
    out[keep + 5] = b'1';
    out
}

/// Candidates [`candidate`] can produce before this crate gives up.
///
/// The first five are the basis and its numeric tails. The rest are distinct
/// hash values, so reaching the end means 59 different 16-bit hashes all
/// collided in one directory — which takes a directory built to make it
/// happen, and [`Error::NoSpace`] is the honest answer to that.
pub const MAX_SHORT_NAME_CANDIDATES: u32 = 64;

fn short_len(field: &[u8]) -> usize {
    field.iter().position(|&b| b == b' ').unwrap_or(field.len())
}

fn hex_digit(v: u8) -> u8 {
    if v < 10 {
        b'0' + v
    } else {
        b'A' + (v - 10)
    }
}

fn fnv16(s: &str) -> u16 {
    let mut h: u32 = 0x811C_9DC5;
    for unit in s.encode_utf16() {
        for b in unit.to_le_bytes() {
            h ^= b as u32;
            h = h.wrapping_mul(0x0100_0193);
        }
    }
    (h ^ (h >> 16)) as u16
}

/// Split a name into the 13-unit groups a long-name entry holds, padded with
/// the format's terminator and filler.
///
/// Returns the groups in *logical* order; the caller writes them to disk in
/// reverse, which is the order the format stores them in.
pub fn lfn_groups(name: &str) -> Result<(usize, [[u16; UNITS_PER_LFN_ENTRY]; MAX_LFN_ENTRIES]), Error> {
    // Sized by the entries rather than by [`MAX_LFN_CHARS`], which is five
    // units smaller: the loop below indexes by group, so a buffer sized to the
    // *name* limit would be in bounds only by an argument about which branch
    // runs, and that is not the kind of thing to leave to an argument.
    let mut units = [0u16; MAX_LFN_ENTRIES * UNITS_PER_LFN_ENTRY];
    let mut len = 0;
    for u in name.encode_utf16() {
        if len == MAX_LFN_CHARS {
            return Err(Error::InvalidName);
        }
        units[len] = u;
        len += 1;
    }
    if len == 0 {
        return Err(Error::InvalidName);
    }

    let groups = len.div_ceil(UNITS_PER_LFN_ENTRY);
    let mut out = [[0xFFFFu16; UNITS_PER_LFN_ENTRY]; MAX_LFN_ENTRIES];
    for g in 0..groups {
        for i in 0..UNITS_PER_LFN_ENTRY {
            let idx = g * UNITS_PER_LFN_ENTRY + i;
            out[g][i] = if idx < len {
                units[idx]
            } else if idx == len {
                0x0000
            } else {
                0xFFFF
            };
        }
    }
    Ok((groups, out))
}

/// Decode UTF-16 units into a `String`, substituting for anything that is not
/// a character.
///
/// Lone surrogates are what a hostile or truncated long-name run produces, and
/// they are not encodable as UTF-8. Substituting keeps the name readable and
/// keeps the file reachable by *some* name; failing would make one broken
/// entry hide every entry after it in a listing.
pub fn units_to_string(units: &[u16]) -> String {
    char::decode_utf16(units.iter().copied())
        .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    /// This asserts only that the rotation is there and that the function is
    /// not constant. The checksum's *correctness* is not decidable here — it
    /// is decided in the host cross-validation tests, where every long name
    /// macOS wrote is read back through this function, and a wrong rotation
    /// makes every one of them fail its checksum and fall back to 8.3.
    #[test]
    fn checksum_rotates_and_is_not_constant() {
        let mut seen = Vec::new();
        for i in 0..11 {
            let mut short: ShortName = *b"           ";
            short[i] = b'A';
            let c = lfn_checksum(&short);
            assert!(!seen.contains(&c), "position {i} collides");
            seen.push(c);
        }
        assert_ne!(lfn_checksum(b"HELLO   TXT"), lfn_checksum(b"HELLO   TXU"));
    }

    #[test]
    fn plain_uppercase_names_need_no_long_entry() {
        for name in ["KERNEL.EFI", "BOOTX64.EFI", "A", "README", "X.Y"] {
            let b = basis_name(name);
            assert!(fits_short(name, &b), "{name}");
        }
    }

    #[test]
    fn anything_else_needs_one() {
        for name in ["kernel.efi", "Kernel.EFI", "toolong12345.txt", "a b.txt", "two.dots.txt", "über.txt"] {
            let b = basis_name(name);
            assert!(!fits_short(name, &b), "{name}");
        }
    }

    #[test]
    fn basis_truncates_and_uppercases() {
        let b = basis_name("a very long name.text");
        assert_eq!(&b.short, b"AVERYLONTEX");
        assert!(b.lossy);
    }

    #[test]
    fn candidates_are_distinct_and_well_formed() {
        let long = "boot-0001.log";
        let b = basis_name(long);
        let mut seen = Vec::new();
        for n in 0..MAX_SHORT_NAME_CANDIDATES {
            let c = candidate(&b, long, n);
            assert!(c.iter().all(|&x| short_byte_ok(x) || x == b' '), "{n}: {c:?}");
            assert!(!seen.contains(&c), "duplicate candidate at {n}");
            seen.push(c);
        }
    }

    #[test]
    fn hash_tails_differ_between_long_names() {
        let mut seen = Vec::new();
        for i in 0..500 {
            let name = alloc::format!("boot-{i:04}.log");
            let b = basis_name(&name);
            // The numeric tails collide by construction; the hash tail is
            // where distinctness has to come from.
            seen.push(candidate(&b, &name, 5));
        }
        seen.sort();
        let before = seen.len();
        seen.dedup();
        // Not asserting zero collisions — a 16-bit hash over 500 names has a
        // birthday expectation of a few. The point is that the tail is not
        // constant, which is what makes the probe terminate.
        assert!(seen.len() > before * 9 / 10, "{} distinct of {before}", seen.len());
    }

    #[test]
    fn long_name_comparison_is_ascii_case_insensitive() {
        let stored: Vec<u16> = "Hello.TXT".encode_utf16().collect();
        assert!(long_name_eq(&stored, "hello.txt"));
        assert!(long_name_eq(&stored, "HELLO.TXT"));
        assert!(!long_name_eq(&stored, "hello.tx"));
        assert!(!long_name_eq(&stored, "hello.txtt"));
    }

    #[test]
    fn groups_pad_with_terminator_then_filler() {
        let (n, g) = lfn_groups("ab").unwrap();
        assert_eq!(n, 1);
        assert_eq!(g[0][0], b'a' as u16);
        assert_eq!(g[0][1], b'b' as u16);
        assert_eq!(g[0][2], 0x0000);
        assert_eq!(g[0][3], 0xFFFF);
    }

    /// A name that exactly fills its last entry has no room for the
    /// terminator, and the format does not require one.
    #[test]
    fn exact_multiple_of_thirteen_has_no_terminator() {
        let name = "abcdefghijklm";
        let (n, g) = lfn_groups(name).unwrap();
        assert_eq!(n, 1);
        assert_eq!(g[0][12], b'm' as u16);
    }

    #[test]
    fn rejects_what_fat_cannot_store() {
        for bad in ["", ".", "..", "a/b", "a<b", "a>b", "a:b", "a\"b", "a|b", "a?b", "a*b", "a\\b", "trailing ", "trailing."] {
            assert!(validate_component(bad).is_err(), "{bad:?} accepted");
        }
        for good in ["a", "a.txt", "A Long Name.tar.gz", "über.txt", "~tilde", "a b"] {
            assert!(validate_component(good).is_ok(), "{good:?} rejected");
        }
    }

    #[test]
    fn rejects_names_past_the_format_limit() {
        let ok: String = core::iter::repeat_n('a', 255).collect();
        assert!(validate_component(&ok).is_ok());
        let too_long: String = core::iter::repeat_n('a', 256).collect();
        assert!(validate_component(&too_long).is_err());
        // Astral characters are two UTF-16 units each, so 128 of them is 256.
        let astral: String = core::iter::repeat_n('\u{1F600}', 128).collect();
        assert!(validate_component(&astral).is_err());
    }

    #[test]
    fn lone_surrogates_decode_to_a_name_rather_than_failing() {
        let s = units_to_string(&[b'a' as u16, 0xD800, b'b' as u16]);
        assert_eq!(s.chars().count(), 3);
    }
}
