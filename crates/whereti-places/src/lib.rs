//! Where a photo was taken (PLAN.md §53), all on this PC.
//!
//! `gps` reads the position a camera or phone wrote into the photo (EXIF),
//! and `place_of` names it: the nearest town or city, its region and its
//! country, from a list of 171,000 places with 1,000+ people (GeoNames, CC BY
//! 4.0) built into whereti by tools/places/build.py. Countries carry their
//! names in whereti's 8 languages, big cities their names in other languages
//! too, so "Roma", "Rome" and "Rzym" all find a photo taken in Rome.

use std::collections::HashMap;
use std::io::Read;
use std::sync::OnceLock;

/// The places, zlib-compressed lines (see tools/places/build.py).
static DATA: &[u8] = include_bytes!("../data/places.bin");

/// A place is named only this close; farther out, only its region and
/// country (a photo in the mountains, at sea near the coast).
const CITY_KM: f64 = 25.0;
/// Farther than this from any place: the middle of an ocean, no name.
const REGION_KM: f64 = 300.0;

#[derive(Debug, Clone, PartialEq)]
pub struct Place {
    /// The town or city, its own name first, then its names elsewhere.
    pub city: Vec<String>,
    pub region: Option<String>,
    /// The country, in English first, then in whereti's other languages.
    pub country: Vec<String>,
    /// How far the photo was from the town's centre.
    pub km: f64,
}

impl Place {
    /// "Rome, Lazio, Italy": what to show.
    pub fn label(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if let Some(c) = self.city.first() {
            parts.push(c);
        }
        if let Some(r) = &self.region {
            if !parts.iter().any(|p| p.eq_ignore_ascii_case(r)) {
                parts.push(r);
            }
        }
        if let Some(c) = self.country.first() {
            parts.push(c);
        }
        parts.join(", ")
    }

    /// Every name, for the search index.
    pub fn search_text(&self) -> String {
        let mut words: Vec<&str> = self.city.iter().map(String::as_str).collect();
        words.extend(self.region.as_deref());
        words.extend(self.country.iter().map(String::as_str));
        words.join(" ")
    }
}

struct City {
    lat: f32,
    lon: f32,
    /// How far the place reaches from its centre, from its population.
    radius_km: f32,
    country: u16,
    region: u32,
    names: Box<str>,
}

/// A big city reaches far from the point GeoNames gives for it (Rome's is
/// 3 km from Piazza Navona, the Vatican's nearer): ~10 km for 2 million
/// people, a few hundred metres for a village.
fn radius_km(population: u64) -> f32 {
    ((population as f32).sqrt() / 150.0).min(20.0)
}

struct Places {
    countries: Vec<Vec<String>>,
    country_of: HashMap<String, u16>,
    regions: Vec<String>,
    cities: Vec<City>,
    /// 1°x1° cells -> cities in them.
    grid: HashMap<(i16, i16), Vec<u32>>,
}

const NO_REGION: u32 = u32::MAX;

fn cell(lat: f64, lon: f64) -> (i16, i16) {
    (lat.floor() as i16, lon.floor() as i16)
}

fn places() -> &'static Places {
    static P: OnceLock<Places> = OnceLock::new();
    P.get_or_init(|| {
        let mut text = String::new();
        let _ = flate2::read::ZlibDecoder::new(DATA).read_to_string(&mut text);
        let mut p = Places {
            countries: Vec::new(),
            country_of: HashMap::new(),
            regions: Vec::new(),
            cities: Vec::new(),
            grid: HashMap::new(),
        };
        let mut region_of: HashMap<&str, u32> = HashMap::new();
        for line in text.lines() {
            let mut cols = line.split('\t');
            match cols.next() {
                Some("C") => {
                    let (Some(code), Some(names)) = (cols.next(), cols.next()) else { continue };
                    p.country_of.insert(code.to_string(), p.countries.len() as u16);
                    p.countries.push(names.split('|').map(String::from).collect());
                }
                Some("A") => {
                    let (Some(code), Some(name)) = (cols.next(), cols.next()) else { continue };
                    region_of.insert(code, p.regions.len() as u32);
                    p.regions.push(name.to_string());
                }
                Some("P") => {
                    let (Some(lat), Some(lon), Some(cc), Some(admin1), Some(population), Some(names)) =
                        (cols.next(), cols.next(), cols.next(), cols.next(), cols.next(), cols.next())
                    else {
                        continue;
                    };
                    let radius_km = radius_km(population.parse().unwrap_or(0));
                    let (Ok(lat), Ok(lon)) = (lat.parse::<f64>(), lon.parse::<f64>()) else { continue };
                    let Some(&country) = p.country_of.get(cc) else { continue };
                    let region = region_of.get(format!("{cc}.{admin1}").as_str()).copied().unwrap_or(NO_REGION);
                    let i = p.cities.len() as u32;
                    p.cities.push(City { lat: lat as f32, lon: lon as f32, radius_km, country, region, names: names.into() });
                    p.grid.entry(cell(lat, lon)).or_default().push(i);
                }
                _ => {}
            }
        }
        p
    })
}

/// Great-circle distance in km.
fn km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = p2 - p1;
    let dl = (lon2 - lon1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * 6371.0 * a.sqrt().asin()
}

/// Rings of cells looked at, at most (near the poles a cell is only a few
/// km wide).
const MAX_RING: i16 = 60;

/// How far (km) the cells up to `ring` around the cell at `clat` surely
/// reach from any point of it. A place `d` km away is at most d/111
/// degrees of latitude off, so `ring` covers it in latitude while
/// d <= ring x 111, and in longitude while d <= ring x (a degree of
/// longitude at the farthest latitude it can have): the largest such d.
fn ring_reach(clat: i16, ring: i16) -> f64 {
    let pole_side = (clat as f64).abs().max((clat as f64 + 1.0).abs());
    let fits = |d: f64| {
        let lat = (pole_side + d / 111.0).min(89.9);
        d <= ring as f64 * 111.0 && d <= ring as f64 * 111.0 * lat.to_radians().cos()
    };
    let (mut lo, mut hi) = (0.0, ring as f64 * 111.0);
    for _ in 0..30 {
        let mid = (lo + hi) / 2.0;
        if fits(mid) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    lo
}

/// The town, region and country at a position.
pub fn place_of(lat: f64, lon: f64) -> Option<Place> {
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return None;
    }
    let p = places();
    let (clat, clon) = cell(lat, lon);
    // (distance to the place's edge, to its centre, which)
    let mut best: Option<(f64, f64, u32)> = None;
    // the cells around, wider until something is found; a degree of
    // longitude is narrow far from the equator, so more rings there
    for ring in 0..=MAX_RING {
        for dlat in -ring..=ring {
            for dlon in -ring..=ring {
                if dlat.abs() != ring && dlon.abs() != ring {
                    continue; // inside: done in an earlier ring
                }
                let mut lo = clon + dlon;
                if lo < -180 {
                    lo += 360;
                } else if lo >= 180 {
                    lo -= 360;
                }
                for &i in p.grid.get(&(clat + dlat, lo)).map(Vec::as_slice).unwrap_or(&[]) {
                    let c = &p.cities[i as usize];
                    let d = km(lat, lon, c.lat as f64, c.lon as f64);
                    let edge = d - c.radius_km as f64;
                    if best.is_none_or(|b| edge < b.0) {
                        best = Some((edge, d, i));
                    }
                }
            }
        }
        // a nearer place cannot be further out than this ring reaches
        let reach = ring_reach(clat, ring);
        if ring >= 1 && (best.is_some_and(|b| b.0 + 20.0 < reach) || reach > REGION_KM + 20.0) {
            break;
        }
    }
    let (edge, d, i) = best?;
    if d > REGION_KM {
        return None;
    }
    let c = &p.cities[i as usize];
    Some(Place {
        city: if edge <= CITY_KM { c.names.split('|').map(String::from).collect() } else { Vec::new() },
        region: (c.region != NO_REGION).then(|| p.regions[c.region as usize].clone()),
        country: p.countries[c.country as usize].clone(),
        km: d,
    })
}

/// The GPS position a camera or phone wrote into a photo (JPEG, HEIC/HEIF,
/// TIFF, PNG, WebP): `(latitude, longitude)` in degrees. The bytes are the
/// file's start (EXIF lives there in JPEG) or, for HEIC, the whole file.
/// Never panics on a damaged file.
pub fn gps(bytes: &[u8]) -> Option<(f64, f64)> {
    std::panic::catch_unwind(|| gps_inner(bytes)).ok().flatten()
}

fn gps_inner(bytes: &[u8]) -> Option<(f64, f64)> {
    use exif::{In, Tag, Value};
    // Tolerant: some apps write small mistakes (a stray "next IFD" pointer)
    // around a GPS position that is fine; keep what can be read.
    let exif = exif::Reader::new()
        .continue_on_error(true)
        .read_from_container(&mut std::io::Cursor::new(bytes))
        .or_else(|e| e.distill_partial_result(|_| {}))
        .ok()?;
    let degrees = |tag: Tag| -> Option<f64> {
        match &exif.get_field(tag, In::PRIMARY)?.value {
            Value::Rational(v) if v.len() >= 3 && v.iter().all(|r| r.denom != 0) => {
                Some(v[0].to_f64() + v[1].to_f64() / 60.0 + v[2].to_f64() / 3600.0)
            }
            _ => None,
        }
    };
    let negative = |tag: Tag, neg: u8| -> bool {
        exif.get_field(tag, In::PRIMARY).is_some_and(|f| match &f.value {
            Value::Ascii(v) => v.first().and_then(|s| s.first()).is_some_and(|c| c.to_ascii_uppercase() == neg),
            _ => false,
        })
    };
    let mut lat = degrees(Tag::GPSLatitude)?;
    let mut lon = degrees(Tag::GPSLongitude)?;
    if negative(Tag::GPSLatitudeRef, b'S') {
        lat = -lat;
    }
    if negative(Tag::GPSLongitudeRef, b'W') {
        lon = -lon;
    }
    // 0,0 is what some apps write when they have no position
    let valid = (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon) && (lat.abs() > 1e-6 || lon.abs() > 1e-6);
    valid.then_some((lat, lon))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_places_are_named() {
        let rome = place_of(41.8986, 12.4769).unwrap(); // Piazza Navona
        assert_eq!(rome.label(), "Rome, Lazio, Italy");
        let text = rome.search_text();
        for name in ["Roma", "Rzym", "Italien", "Italia", "Itália", "Włochy"] {
            assert!(text.contains(name), "{name} missing in {text}");
        }
        let bucharest = place_of(44.4268, 26.1025).unwrap();
        assert!(bucharest.label().ends_with("Romania"), "{}", bucharest.label());
        assert!(bucharest.search_text().contains("București") || bucharest.search_text().contains("Bucureşti"));
        let brasov = place_of(45.6427, 25.5887).unwrap();
        assert!(brasov.city[0].starts_with("Bra"), "{:?}", brasov.city);
        // near the date line, and in the south and west
        assert!(place_of(-36.8485, 174.7633).unwrap().label().contains("New Zealand"));
        assert!(place_of(-22.9068, -43.1729).unwrap().label().contains("Brazil"));
    }

    #[test]
    fn rings_reach_less_far_from_the_equator() {
        // near the equator a ring is a degree: ~111 km
        assert!((ring_reach(0, 1) - 111.0).abs() < 2.0);
        // in the north of Norway a degree of longitude is ~40 km
        let north = ring_reach(69, 2);
        assert!(north > 40.0 && north < 100.0, "{north}");
        assert!(ring_reach(69, 20) > 300.0);
        // the nearest town is found far in the north too (Tromsø)
        assert!(place_of(69.65, 18.96).unwrap().label().contains("Norway"));
    }

    #[test]
    fn far_from_towns_only_the_region_and_country() {
        // the middle of the Atlantic
        assert_eq!(place_of(30.0, -40.0), None);
        // out of range
        assert_eq!(place_of(95.0, 0.0), None);
    }

    /// A JPEG with only an EXIF block holding a GPS position.
    pub(crate) fn jpeg_at(lat: (u32, u32, u32), north: bool, lon: (u32, u32, u32), east: bool) -> Vec<u8> {
        use exif::experimental::Writer;
        use exif::{Field, In, Rational, Tag, Value};
        let dms = |(d, m, s): (u32, u32, u32)| Value::Rational(vec![Rational::from((d, 1)), Rational::from((m, 1)), Rational::from((s, 100))]);
        let fields = [
            Field { tag: Tag::GPSLatitudeRef, ifd_num: In::PRIMARY, value: Value::Ascii(vec![if north { b"N".to_vec() } else { b"S".to_vec() }]) },
            Field { tag: Tag::GPSLatitude, ifd_num: In::PRIMARY, value: dms(lat) },
            Field { tag: Tag::GPSLongitudeRef, ifd_num: In::PRIMARY, value: Value::Ascii(vec![if east { b"E".to_vec() } else { b"W".to_vec() }]) },
            Field { tag: Tag::GPSLongitude, ifd_num: In::PRIMARY, value: dms(lon) },
        ];
        let mut w = Writer::new();
        for f in &fields {
            w.push_field(f);
        }
        let mut tiff = std::io::Cursor::new(Vec::new());
        w.write(&mut tiff, false).unwrap();
        let tiff = tiff.into_inner();
        let len = (2 + 6 + tiff.len()) as u16;
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE1];
        jpeg.extend(len.to_be_bytes());
        jpeg.extend(b"Exif\0\0");
        jpeg.extend(tiff);
        jpeg.extend([0xFF, 0xD9]);
        jpeg
    }

    #[test]
    fn a_photo_from_rome_is_found_in_rome() {
        // 41°53'54.96"N 12°28'36.84"E, Piazza Navona
        let jpeg = jpeg_at((41, 53, 5496), true, (12, 28, 3684), true);
        let (lat, lon) = gps(&jpeg).expect("gps");
        assert!((lat - 41.8986).abs() < 1e-3 && (lon - 12.4769).abs() < 1e-3, "{lat} {lon}");
        assert_eq!(place_of(lat, lon).unwrap().label(), "Rome, Lazio, Italy");
        // south and west: Rio de Janeiro
        let jpeg = jpeg_at((22, 54, 2448), false, (43, 10, 2244), false);
        let (lat, lon) = gps(&jpeg).unwrap();
        assert!(lat < 0.0 && lon < 0.0);
        assert!(place_of(lat, lon).unwrap().label().contains("Brazil"));
    }

    #[test]
    fn a_photo_without_gps_or_damaged_has_no_position() {
        assert_eq!(gps(b""), None);
        assert_eq!(gps(&[0xFF, 0xD8, 0xFF, 0xE1, 0x00, 0x10, b'E', b'x', b'i', b'f', 0, 0, 1, 2, 3]), None);
        assert_eq!(gps(&vec![0xAB; 4096]), None);
    }
}
