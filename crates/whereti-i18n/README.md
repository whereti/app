# whereti-i18n

Every text of the app and the installer, in every language (PLAN.md §37).
Same approach as SilentSilo (Software Hive S.R.L.).

## How it works

- **English is the key.** Code writes `tr("Search…")`; the text in the current
  language comes from the catalogs.
- **One catalog per screen** in `src/screens/` (search, preview, dates,
  welcome, settings, tray, setup, common). Each entry: the English, a note for
  translators, then every language side by side.
- **Values** go in by name: `trf!("{count} on {drives}", count = n, drives)`.
  Translators may move `{values}`, never rename them.
- **Plurals**: `trn!("{n} file|{n} files", n)`. Forms are split by `|`, as many
  as the language has (`src/plural.rs`): 2 for most, 3 for Romanian
  (1 fișier, 2 fișiere, 20 de fișiere) and Polish (1 plik, 2 pliki, 5 plików).
  French and Portuguese use the first form for 0 too.
- **Numbers** follow the language: 1,234 / 1.234 / 1 234 (`number`).
- **Languages** (`src/locales.rs`) are listed by their own name. Settings
  offers "Same as Windows" first (the default) and marks languages not yet
  read by a native speaker with "beta" (`reviewed: false`).

## Adding or changing a text

1. Write it in the code with `tr`, `trf!` or `trn!`.
2. `cargo test -p whereti-i18n` lists what is missing: add the entry to the
   screen's catalog with a note and every language.
3. The tests also check that every catalog text is used, `{values}` match,
   plural texts have the right number of forms, and there are no long dashes.

## Adding a language

Add it to `Lang`, `LOCALES` (alphabetical by native name), `Entry` and
`plural.rs`, then fill its column in every catalog. It stays "beta" until a
native speaker has read every text.

## Rules for every language

- Plain words for everyone, like Windows 11's own apps; nothing technical.
- Use Windows' own names (Explorer, administrator, notification area).
- whereti is always lowercase. Key names as on the keyboard of that language.
- Short: buttons and status lines have little room.
- No long dashes (— or –): a simple hyphen, a comma, or nothing.

## Glossary

| English | Deutsch | Español | Français | Italiano | Polski | Português (Brasil) | Română |
|---|---|---|---|---|---|---|---|
| index | Index | índice | index | indice | indeks | índice | index |
| file | Datei | archivo | fichier | file | plik | arquivo | fișier |
| folder | Ordner | carpeta | dossier | cartella | folder | pasta | folder |
| document | Dokument | documento | document | documento | dokument | documento | document |
| contents | Inhalt | contenido | contenu | contenuto | zawartość | conteúdo | conținut |
| search | Suche | búsqueda | recherche | ricerca | wyszukiwanie / szukaj | pesquisa | căutare |
| results | Ergebnisse | resultados | résultats | risultati | wyniki | resultados | rezultate |
| shortcut | Tastenkombination | método abreviado | raccourci | scorciatoia | skrót klawiaturowy (krótko: skrót) | atalho | tastă rapidă |
| tray | Infobereich (neben der Uhr) | bandeja del sistema | zone de notification | area di notifica | zasobnik (ikona w zasobniku) | bandeja | tray (lângă ceas) |
| settings | Einstellungen | Configuración | Paramètres | Impostazioni | ustawienia | Configurações | setări |
| folders whereti skips | Ordner, die whereti auslässt | carpetas que whereti omite | Dossiers ignorés par whereti | Cartelle ignorate da whereti | foldery pomijane przez whereti | Pastas que o whereti ignora | foldere pe care whereti le sare |
| preview | Vorschau | vista previa | aperçu | anteprima | podgląd | visualização | previzualizare |
| Explorer | Explorer | Explorador | Explorateur de fichiers (l'Explorateur in the compact status bar) | Esplora file | Eksplorator (Eksplorator plików) | Explorador / Explorador de Arquivos | Explorer |
| drive | Laufwerk | unidad | lecteur | unità | dysk | unidade | partiție |
| start with Windows | Mit Windows starten | iniciar con Windows | Démarrer avec Windows | Avvia con Windows | uruchamiaj z systemem Windows | Iniciar com o Windows | pornește cu Windows |
| up to date | aktuell | actualizado | à jour | aggiornato | aktualny | atualizado | actualizat |
