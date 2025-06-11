//! The tray icon menu and the global shortcut.
//!
//! English is the key; a note for translators; then every language.
//! Plural texts: forms split by `|` (see `plural.rs`).

use crate::Entry;

pub const ENTRIES: &[Entry] = &[
    Entry {
        en: "Search…",
        note: "Tray menu item that opens the search window; shown followed by the shortcut, e.g. \"Search… Ctrl+Shift+Space\"; keep the ellipsis.",
        de: "Suchen…",
        es: "Buscar…",
        fr: "Rechercher…",
        it: "Cerca…",
        pl: "Szukaj…",
        pt_br: "Pesquisar…",
        ro: "Caută…",
    },
    Entry {
        en: "Quit",
        note: "Tray menu item that quits the app.",
        de: "Beenden",
        es: "Salir",
        fr: "Quitter",
        it: "Esci",
        pl: "Zakończ",
        pt_br: "Sair",
        ro: "Ieșire",
    },
    Entry {
        en: "shortcuts are not available",
        note: "Error reason shown in a toast when the shortcut cannot be registered at all; lowercase.",
        de: "Tastenkombinationen sind nicht verfügbar",
        es: "los métodos abreviados no están disponibles",
        fr: "les raccourcis ne sont pas disponibles",
        it: "le scorciatoie non sono disponibili",
        pl: "skróty są niedostępne",
        pt_br: "os atalhos não estão disponíveis",
        ro: "tastele rapide nu sunt disponibile",
    },
    Entry {
        en: "Use Ctrl or Alt with another key",
        note: "Error message shown in a toast when the typed shortcut is invalid; keep key names Ctrl and Alt.",
        de: "Verwenden Sie Strg oder Alt mit einer anderen Taste",
        es: "Usa Ctrl o Alt con otra tecla",
        fr: "Utilisez Ctrl ou Alt avec une autre touche",
        it: "Usa Ctrl o Alt con un altro tasto",
        pl: "Użyj Ctrl lub Alt z innym klawiszem",
        pt_br: "Use Ctrl ou Alt com outra tecla",
        ro: "Folosește Ctrl sau Alt împreună cu altă tastă",
    },
    Entry {
        en: "{label} is used by another app",
        note: "Error message shown in a toast; {label} is a key combination like \"Ctrl+Shift+Space\" already taken by another program.",
        de: "{label} wird von einer anderen App verwendet",
        es: "{label} lo usa otra aplicación",
        fr: "{label} est utilisé par une autre application",
        it: "{label} è usata da un'altra app",
        pl: "Skrót {label} jest zajęty przez inną aplikację",
        pt_br: "{label} é usado por outro app",
        ro: "{label} e folosită de altă aplicație",
    },
];
