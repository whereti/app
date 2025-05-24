//! The languages, in alphabetical order of their own names (as SilentSilo
//! lists them). `reviewed`: a native speaker has read every text; the others
//! show "beta" in Settings until then.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Lang {
    De,
    En,
    Es,
    Fr,
    It,
    Pl,
    PtBr,
    Ro,
}

pub struct Locale {
    pub lang: Lang,
    /// BCP 47, also what the settings file stores.
    pub code: &'static str,
    /// The language's name in that language.
    pub native: &'static str,
    pub reviewed: bool,
    /// How many forms a `trn!` text has: "{n} file|{n} files" has 2.
    pub plural_forms: usize,
    pub thousands: char,
    pub decimal: char,
    /// Digits before the first separator is used: 2 = "1234" but "12 345".
    pub min_grouping: u8,
}

const NBSP: char = '\u{a0}';

pub const LOCALES: [Locale; 8] = [
    Locale {
        lang: Lang::De,
        code: "de",
        native: "Deutsch",
        reviewed: false,
        plural_forms: 2,
        thousands: '.',
        decimal: ',',
        min_grouping: 1,
    },
    Locale {
        lang: Lang::En,
        code: "en",
        native: "English",
        reviewed: true,
        plural_forms: 2,
        thousands: ',',
        decimal: '.',
        min_grouping: 1,
    },
    Locale {
        lang: Lang::Es,
        code: "es",
        native: "Español",
        reviewed: false,
        plural_forms: 2,
        thousands: '.',
        decimal: ',',
        min_grouping: 2,
    },
    Locale {
        lang: Lang::Fr,
        code: "fr",
        native: "Français",
        reviewed: false,
        plural_forms: 2,
        thousands: NBSP,
        decimal: ',',
        min_grouping: 1,
    },
    Locale {
        lang: Lang::It,
        code: "it",
        native: "Italiano",
        reviewed: false,
        plural_forms: 2,
        thousands: '.',
        decimal: ',',
        min_grouping: 1,
    },
    Locale {
        lang: Lang::Pl,
        code: "pl",
        native: "Polski",
        reviewed: false,
        plural_forms: 3,
        thousands: NBSP,
        decimal: ',',
        min_grouping: 2,
    },
    Locale {
        lang: Lang::PtBr,
        code: "pt-BR",
        native: "Português (Brasil)",
        reviewed: false,
        plural_forms: 2,
        thousands: '.',
        decimal: ',',
        min_grouping: 1,
    },
    Locale {
        lang: Lang::Ro,
        code: "ro",
        native: "Română",
        reviewed: true,
        plural_forms: 3,
        thousands: '.',
        decimal: ',',
        min_grouping: 1,
    },
];

impl Lang {
    pub const ALL: [Lang; 8] = [
        Lang::De,
        Lang::En,
        Lang::Es,
        Lang::Fr,
        Lang::It,
        Lang::Pl,
        Lang::PtBr,
        Lang::Ro,
    ];

    pub fn locale(self) -> &'static Locale {
        &LOCALES[self as usize]
    }

    /// "ro-RO", "pt", "pt-PT", "en-US"… to one of ours; `None` if we lack it.
    /// Any Portuguese gets Brazilian Portuguese.
    pub fn from_code(code: &str) -> Option<Lang> {
        let code = code.trim().to_ascii_lowercase();
        let primary = code.split(['-', '_']).next().unwrap_or("");
        LOCALES
            .iter()
            .find(|l| l.code.split('-').next() == Some(primary))
            .map(|l| l.lang)
    }

    /// The first of the user's Windows display languages that whereti has,
    /// else English.
    pub fn system() -> Lang {
        windows_languages()
            .iter()
            .find_map(|c| Lang::from_code(c))
            .unwrap_or(Lang::En)
    }
}

/// The user's display languages in order of preference, e.g. ["ro-RO", "en-US"].
fn windows_languages() -> Vec<String> {
    use windows_sys::Win32::Globalization::{GetUserPreferredUILanguages, MUI_LANGUAGE_NAME};
    let (mut count, mut len) = (0u32, 0u32);
    // SAFETY: the first call asks for the size, the second fills a buffer of
    // exactly that size.
    unsafe {
        if GetUserPreferredUILanguages(
            MUI_LANGUAGE_NAME,
            &mut count,
            std::ptr::null_mut(),
            &mut len,
        ) == 0
            || len == 0
        {
            return Vec::new();
        }
        let mut buf = vec![0u16; len as usize];
        if GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &mut count, buf.as_mut_ptr(), &mut len)
            == 0
        {
            return Vec::new();
        }
        buf.split(|&c| c == 0)
            .filter(|s| !s.is_empty())
            .map(String::from_utf16_lossy)
            .collect()
    }
}
