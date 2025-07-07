//! Texts used on more than one screen.
//!
//! English is the key; a note for translators; then every language.
//! Plural texts: forms split by `|` (see `plural.rs`).

use crate::Entry;

pub const ENTRIES: &[Entry] = &[
    Entry {
        en: "The whereti service is not running",
        note: "Title of an error message card in the search window, shown when the background search service cannot be reached.",
        de: "Der whereti-Dienst wird nicht ausgeführt",
        es: "El servicio whereti no se está ejecutando",
        fr: "Le service whereti n'est pas en cours d'exécution",
        it: "Il servizio whereti non è in esecuzione",
        pl: "Usługa whereti nie działa",
        pt_br: "O serviço do whereti não está em execução",
        ro: "Serviciul whereti nu rulează",
    },
    Entry {
        en: "connecting…",
        note: "Status text in the Settings > Search rows and status bar while whereti is connecting to its background service; keep the ellipsis.",
        de: "Verbindung wird hergestellt…",
        es: "conectando…",
        fr: "connexion…",
        it: "connessione…",
        pl: "łączenie…",
        pt_br: "conectando…",
        ro: "se conectează…",
    },
    Entry {
        en: "About",
        note: "Name of the Windows 11 Settings page opened by ms-settings:about. Use the exact name Windows 11 shows in your language.",
        de: "Info",
        es: "Información",
        fr: "Informations système",
        it: "Informazioni su",
        pl: "Informacje",
        pt_br: "Sobre",
        ro: "Despre",
    },
    Entry {
        en: "Try again",
        note: "Settings > About, button after a failed update.",
        de: "Erneut versuchen",
        es: "Intentar de nuevo",
        fr: "Réessayer",
        it: "Riprova",
        pl: "Spróbuj ponownie",
        pt_br: "Tentar novamente",
        ro: "Încearcă din nou",
    },
    Entry {
        en: "Cancel",
        note: "Button label that stops recording a new keyboard shortcut (Settings > General, Shortcut row) or cancels in the installer; short.",
        de: "Abbrechen",
        es: "Cancelar",
        fr: "Annuler",
        it: "Annulla",
        pl: "Anuluj",
        pt_br: "Cancelar",
        ro: "Renunță",
    },
];
