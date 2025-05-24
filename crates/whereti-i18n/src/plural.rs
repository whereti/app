//! Which form of a `trn!` text a number takes (CLDR plural rules, whole
//! numbers only).
//!
//! - 2 forms (one | other): English, German, Spanish, Italian: 1 file, 0/2 files.
//!   French and Portuguese (Brazil) also use "one" for 0: 0 fichier.
//! - Romanian, 3 forms (one | few | other): 1 fișier, 2..19 fișiere (also 0,
//!   and 101..119), 20 de fișiere.
//! - Polish, 3 forms (one | few | many): 1 plik, 2..4 pliki (22..24, …),
//!   5 plików (also 0, 12..14).

use crate::Lang;

pub fn form_index(lang: Lang, n: u64) -> usize {
    match lang {
        Lang::De | Lang::En | Lang::Es | Lang::It => usize::from(n != 1),
        Lang::Fr | Lang::PtBr => usize::from(n > 1),
        Lang::Ro => match n {
            1 => 0,
            _ if n == 0 || (1..=19).contains(&(n % 100)) => 1,
            _ => 2,
        },
        Lang::Pl => match n {
            1 => 0,
            _ if (2..=4).contains(&(n % 10)) && !(12..=14).contains(&(n % 100)) => 1,
            _ => 2,
        },
    }
}
