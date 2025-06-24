//! Updates: the daily check, the download and the install.
//!
//! English is the key; a note for translators; then every language.
//! Plural texts: forms split by `|` (see `plural.rs`).

use crate::Entry;

pub const ENTRIES: &[Entry] = &[
    Entry {
        en: "The download failed (error {status}).",
        note: "Update error; {status} is an HTTP status number like 404.",
        de: "Der Download ist fehlgeschlagen (Fehler {status}).",
        es: "La descarga falló (error {status}).",
        fr: "Le téléchargement a échoué (erreur {status}).",
        it: "Il download non è riuscito (errore {status}).",
        pl: "Pobieranie nie powiodło się (błąd {status}).",
        pt_br: "O download falhou (erro {status}).",
        ro: "Descărcarea a eșuat (eroarea {status}).",
    },
    Entry {
        en: "The download is damaged. Try again later.",
        note: "Update error when the downloaded installer does not match its checksum.",
        de: "Der Download ist beschädigt. Versuchen Sie es später erneut.",
        es: "La descarga está dañada. Inténtalo más tarde.",
        fr: "Le téléchargement est endommagé. Réessayez plus tard.",
        it: "Il download è danneggiato. Riprova più tardi.",
        pl: "Pobrany plik jest uszkodzony. Spróbuj później.",
        pt_br: "O download está danificado. Tente mais tarde.",
        ro: "Descărcarea e deteriorată. Încearcă mai târziu.",
    },
    Entry {
        en: "The update is not signed by {publisher}: {e}",
        note: "Update error: the installer's digital signature is missing or from someone else. {publisher} is 'Software Hive S.R.L.', {e} a technical detail.",
        de: "Das Update ist nicht von {publisher} signiert: {e}",
        es: "La actualización no está firmada por {publisher}: {e}",
        fr: "La mise à jour n'est pas signée par {publisher} : {e}",
        it: "L'aggiornamento non è firmato da {publisher}: {e}",
        pl: "Aktualizacja nie jest podpisana przez {publisher}: {e}",
        pt_br: "A atualização não está assinada por {publisher}: {e}",
        ro: "Actualizarea nu e semnată de {publisher}: {e}",
    },
    Entry {
        en: "The installer could not be started.",
        note: "Update error.",
        de: "Das Installationsprogramm konnte nicht gestartet werden.",
        es: "No se pudo iniciar el instalador.",
        fr: "Le programme d'installation n'a pas pu démarrer.",
        it: "Impossibile avviare il programma di installazione.",
        pl: "Nie udało się uruchomić instalatora.",
        pt_br: "Não foi possível iniciar o instalador.",
        ro: "Programul de instalare nu a putut porni.",
    },
    Entry {
        en: "The update was cancelled or did not finish.",
        note: "Update error, e.g. when the Windows permission prompt was declined.",
        de: "Das Update wurde abgebrochen oder nicht abgeschlossen.",
        es: "La actualización se canceló o no terminó.",
        fr: "La mise à jour a été annulée ou ne s'est pas terminée.",
        it: "L'aggiornamento è stato annullato o non è terminato.",
        pl: "Aktualizacja została anulowana lub nie została ukończona.",
        pt_br: "A atualização foi cancelada ou não terminou.",
        ro: "Actualizarea a fost anulată sau nu s-a terminat.",
    },
];
