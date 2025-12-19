"""Builds crates/whereti-places/data/places.bin (PLAN.md §53): the places
whereti names a photo's GPS position with, offline.

Sources (download into one folder first):
  https://download.geonames.org/export/dump/cities1000.zip   (unzipped: cities1000.txt)
  https://download.geonames.org/export/dump/admin1CodesASCII.txt
  https://download.geonames.org/export/dump/countryInfo.txt
GeoNames data is CC BY 4.0 (https://www.geonames.org). Country names in the
8 languages of whereti come from CLDR, through Babel (pip install babel).

  python tools/places/build.py <folder with the GeoNames files>

Output, zlib-compressed UTF-8 lines:
  C <tab> code <tab> name|name|...        a country, its names in the 8 languages
  A <tab> CC.code <tab> name               a region (first-level division)
  P <tab> lat <tab> lon <tab> CC <tab> admin1 <tab> population <tab> name|other names...
Neighbourhoods are left out: a photo is named after its town or city.
Places with 100,000+ people also carry their names in other languages
(Latin script only: what a person types), so "Rom", "Roma" and "Rzym" all
find Rome.
"""

import os
import re
import sys
import zlib

from babel import Locale

LANGS = ["en", "de", "es", "fr", "it", "pl", "pt_BR", "ro"]
BIG_CITY = 100_000
MAX_OTHER_NAMES = 24
LATIN = re.compile(r"^[A-Za-zÀ-ɏḀ-ỿ' .\-]+$")


def main(src: str, out: str) -> None:
    lines = []
    locales = [Locale.parse(l) for l in LANGS]
    countries = set()
    with open(os.path.join(src, "countryInfo.txt"), encoding="utf-8") as f:
        for row in f:
            if row.startswith("#"):
                continue
            cols = row.rstrip("\n").split("\t")
            code, english = cols[0], cols[4]
            names = [english]
            for loc in locales:
                n = loc.territories.get(code)
                if n and n not in names:
                    names.append(n)
            countries.add(code)
            lines.append(f"C\t{code}\t{'|'.join(names)}")
    with open(os.path.join(src, "admin1CodesASCII.txt"), encoding="utf-8") as f:
        for row in f:
            cols = row.rstrip("\n").split("\t")
            if len(cols) >= 2 and cols[1]:
                lines.append(f"A\t{cols[0]}\t{cols[1]}")
    places = 0
    with open(os.path.join(src, "cities1000.txt"), encoding="utf-8") as f:
        for row in f:
            cols = row.rstrip("\n").split("\t")
            # a neighbourhood (PPLX) would name a photo in Rome "Sant'Eustachio"
            if cols[7] == "PPLX":
                continue
            name, ascii_name, alternates = cols[1], cols[2], cols[3]
            lat, lon, cc, admin1, population = cols[4], cols[5], cols[8], cols[10], int(cols[14] or 0)
            names = [name]
            if ascii_name and ascii_name.lower() != name.lower():
                names.append(ascii_name)
            if population >= BIG_CITY:
                seen = {n.lower() for n in names}
                for a in alternates.split(","):
                    a = a.strip()
                    if 2 < len(a) <= 40 and LATIN.match(a) and a.lower() not in seen:
                        seen.add(a.lower())
                        names.append(a)
                    if len(names) >= MAX_OTHER_NAMES:
                        break
            lines.append(f"P\t{float(lat):.4f}\t{float(lon):.4f}\t{cc}\t{admin1}\t{population}\t{'|'.join(names)}")
            places += 1
    raw = ("\n".join(lines) + "\n").encode("utf-8")
    packed = zlib.compress(raw, 9)
    os.makedirs(os.path.dirname(out), exist_ok=True)
    with open(out, "wb") as f:
        f.write(packed)
    print(f"{places} places, {len(countries)} countries: {len(raw) >> 10} KB, {len(packed) >> 10} KB packed -> {out}")


if __name__ == "__main__":
    here = os.path.dirname(os.path.abspath(__file__))
    main(sys.argv[1], os.path.join(here, "..", "..", "crates", "whereti-places", "data", "places.bin"))
