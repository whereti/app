//! Updates: the daily look for a newer version, got from whereti.com.
//!
//! English is the key; a note for translators; then every language.
//! Plural texts: forms split by `|` (see `plural.rs`).

use crate::Entry;

pub const ENTRIES: &[Entry] = &[
    Entry {
        en: "whereti {version} is out",
        note: "Title of the banner above the results, and of the row in Settings > About, when a newer version of whereti is published; {version} is like 1.3.0. whereti does not download it: the user gets it from the website.",
        de: "whereti {version} ist erschienen",
        es: "Ya está aquí whereti {version}",
        fr: "whereti {version} est sorti",
        it: "È uscito whereti {version}",
        pl: "Jest już whereti {version}",
        pt_br: "Saiu o whereti {version}",
        ro: "A apărut whereti {version}",
    },
    Entry {
        en: "New features and fixes. Download it from whereti.com: your settings and index stay as they are.",
        note: "Text of the new-version banner when the release has no line of its own. The user downloads the installer from the website and runs it over the installed version.",
        de: "Neue Funktionen und Korrekturen. Laden Sie es von whereti.com herunter: Ihre Einstellungen und Ihr Index bleiben erhalten.",
        es: "Novedades y correcciones. Descárgalo de whereti.com: tu configuración y tu índice se conservan.",
        fr: "Nouveautés et corrections. Téléchargez-le sur whereti.com : vos paramètres et votre index restent tels quels.",
        it: "Novità e correzioni. Scaricalo da whereti.com: le impostazioni e l'indice restano come sono.",
        pl: "Nowe funkcje i poprawki. Pobierz go z whereti.com: ustawienia i indeks zostają bez zmian.",
        pt_br: "Novidades e correções. Baixe em whereti.com: suas configurações e seu índice continuam como estão.",
        ro: "Funcții noi și corecturi. Descarcă-l de pe whereti.com: setările și indexul rămân cum sunt.",
    },
    Entry {
        en: "Download it from whereti.com: your settings and index stay as they are.",
        note: "Settings > About: the line under \"whereti {version} is out\" when the release has no line of its own.",
        de: "Laden Sie es von whereti.com herunter: Ihre Einstellungen und Ihr Index bleiben erhalten.",
        es: "Descárgalo de whereti.com: tu configuración y tu índice se conservan.",
        fr: "Téléchargez-le sur whereti.com : vos paramètres et votre index restent tels quels.",
        it: "Scaricalo da whereti.com: le impostazioni e l'indice restano come sono.",
        pl: "Pobierz go z whereti.com: ustawienia i indeks zostają bez zmian.",
        pt_br: "Baixe em whereti.com: suas configurações e seu índice continuam como estão.",
        ro: "Descarcă-l de pe whereti.com: setările și indexul rămân cum sunt.",
    },
    Entry {
        en: "See what's new",
        note: "Button on the new-version banner and in Settings > About: opens the page about the new version on whereti.com, where it is downloaded.",
        de: "Was ist neu",
        es: "Ver las novedades",
        fr: "Voir les nouveautés",
        it: "Vedi le novità",
        pl: "Zobacz nowości",
        pt_br: "Ver as novidades",
        ro: "Vezi ce e nou",
    },
    Entry {
        en: "In a week",
        note: "Second button on the new-version banner: hide it for seven days (\"remind me in a week\").",
        de: "In einer Woche",
        es: "Dentro de una semana",
        fr: "Dans une semaine",
        it: "Tra una settimana",
        pl: "Za tydzień",
        pt_br: "Daqui a uma semana",
        ro: "Peste o săptămână",
    },
    Entry {
        en: "whereti {version} is out · See what's new",
        note: "Clickable line at the bottom of the search window, instead of the banner once it was put off; {version} is like 1.3.0. Opens the page about it on whereti.com.",
        de: "whereti {version} ist erschienen · Was ist neu",
        es: "Ya está aquí whereti {version} · Ver las novedades",
        fr: "whereti {version} est sorti · Voir les nouveautés",
        it: "È uscito whereti {version} · Vedi le novità",
        pl: "Jest już whereti {version} · Zobacz nowości",
        pt_br: "Saiu o whereti {version} · Ver as novidades",
        ro: "A apărut whereti {version} · Vezi ce e nou",
    },
];
