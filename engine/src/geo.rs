//! Geo search, as in Algolia: records carry `_geoloc`, one point
//! (`{"lat": 48.85, "lng": 2.35}`) or a list of them, and searches can
//!
//! - rank by distance from `aroundLatLng` (the `geo` criterion), counting
//!   distances in steps of `aroundPrecision` meters (default 10), so records
//!   a few meters apart tie and the next criteria decide;
//! - keep only records within `aroundRadius` meters of it. Without a radius
//!   one is chosen automatically: just wide enough for the 1,000 nearest
//!   matching records (at least `minimumAroundRadius`). `"all"` keeps every
//!   record, those without `_geoloc` last;
//! - keep only records inside `insideBoundingBox` or `insidePolygon` areas,
//!   which then take the place of the radius.
//!
//! A record's distance is that of its nearest point. Points are kept per
//! record and in a grid of cells of `CELL_DEGREES`, so an area query only
//! looks at the cells it overlaps. Boxes and polygons are taken on the
//! latitude/longitude plane and must not cross the 180th meridian.

use crate::store::DocId;
use roaring::RoaringBitmap;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Mean Earth radius, in meters.
const EARTH_RADIUS: f64 = 6_371_000.0;
/// Size of a grid cell, in degrees (about 28 km of latitude).
const CELL_DEGREES: f64 = 0.25;
/// Cells looked up before scanning every point is cheaper.
const MAX_CELLS: usize = 20_000;
/// Records the automatic radius aims for.
pub const AUTOMATIC_RADIUS_HITS: usize = 1000;
/// `aroundPrecision` when none is given, in meters.
pub const DEFAULT_PRECISION: f64 = 10.0;

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub struct GeoPoint {
    pub lat: f64,
    pub lng: f64,
}

impl GeoPoint {
    fn new(lat: f64, lng: f64) -> Option<Self> {
        ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lng)).then_some(GeoPoint { lat, lng })
    }

    /// Great-circle distance in meters (haversine).
    pub fn distance(&self, other: &GeoPoint) -> f64 {
        let (lat1, lat2) = (self.lat.to_radians(), other.lat.to_radians());
        let dlat = lat2 - lat1;
        let dlng = (other.lng - self.lng).to_radians();
        let h = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlng / 2.0).sin().powi(2);
        2.0 * EARTH_RADIUS * h.sqrt().min(1.0).asin()
    }

    fn cell(&self) -> (i32, i32) {
        ((self.lat / CELL_DEGREES).floor() as i32, lng_cell((self.lng / CELL_DEGREES).floor() as i64))
    }
}

/// Longitude cells around the globe.
const LNG_CELLS: i64 = (360.0 / CELL_DEGREES) as i64;

/// A longitude cell index brought into -180..180 degrees (180 is -180).
fn lng_cell(j: i64) -> i32 {
    ((j + LNG_CELLS / 2).rem_euclid(LNG_CELLS) - LNG_CELLS / 2) as i32
}

/// Rings of cells searched by `nearest_k` before measuring every record.
const MAX_RINGS: i64 = 200;
/// Below this many records, `nearest_k` measures every one.
const SCAN_BELOW: u64 = 2000;

fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// The points of a record's `_geoloc`; invalid entries are skipped.
fn points_of(doc: &Map<String, Value>) -> Vec<GeoPoint> {
    let point = |v: &Value| GeoPoint::new(number(v.get("lat")?)?, number(v.get("lng")?)?);
    match doc.get("_geoloc") {
        Some(Value::Array(items)) => items.iter().filter_map(point).collect(),
        Some(v) => point(v).into_iter().collect(),
        None => Vec::new(),
    }
}

/// `"lat, lng"`, as in `aroundLatLng`.
pub fn parse_point(text: &str) -> Result<GeoPoint, String> {
    let bad = || format!("aroundLatLng must be \"lat, lng\", got {text:?}");
    let (lat, lng) = text.split_once(',').ok_or_else(bad)?;
    let (lat, lng) = (lat.trim().parse().map_err(|_| bad())?, lng.trim().parse().map_err(|_| bad())?);
    GeoPoint::new(lat, lng).ok_or_else(bad)
}

/// Areas as Algolia accepts them: `[[a, b, c, d], ...]`, one flat list of
/// numbers, or the same as text (`"a,b,c,d"`). Each area has `size`
/// numbers (4 for a box), or at least `size` and an even count (polygons).
pub fn parse_areas(value: &Value, name: &str, size: usize, exact: bool) -> Result<Vec<Vec<f64>>, String> {
    let bad = || format!("{name} must be a list of {size}{} numbers per area", if exact { "" } else { " or more" });
    let numbers = |v: &Value| -> Result<Vec<f64>, String> {
        match v {
            Value::Array(items) => items.iter().map(|n| number(n).ok_or_else(bad)).collect(),
            Value::String(s) => s.split(',').map(|n| n.trim().parse().map_err(|_| bad())).collect(),
            _ => Err(bad()),
        }
    };
    let areas = match value {
        Value::Array(items) if items.iter().all(|i| i.is_array()) => items.iter().map(numbers).collect::<Result<_, _>>()?,
        Value::String(s) if s.trim_start().starts_with('[') => {
            let parsed: Value = serde_json::from_str(s).map_err(|_| bad())?;
            return parse_areas(&parsed, name, size, exact);
        }
        other => vec![numbers(other)?],
    };
    let ok = |a: &Vec<f64>| if exact { a.len() == size } else { a.len() >= size && a.len().is_multiple_of(2) };
    if areas.is_empty() || !areas.iter().all(ok) {
        return Err(bad());
    }
    Ok(areas)
}

/// Whether `p` is inside the polygon `[lat1, lng1, lat2, lng2, ...]`
/// (ray casting on the latitude/longitude plane).
fn in_polygon(p: &GeoPoint, polygon: &[f64]) -> bool {
    let vertices: Vec<(f64, f64)> = polygon.chunks(2).map(|c| (c[0], c[1])).collect();
    let mut inside = false;
    let mut j = vertices.len() - 1;
    for i in 0..vertices.len() {
        let ((lat_i, lng_i), (lat_j, lng_j)) = (vertices[i], vertices[j]);
        if (lng_i > p.lng) != (lng_j > p.lng) && p.lat < (lat_j - lat_i) * (p.lng - lng_i) / (lng_j - lng_i) + lat_i {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Latitude and longitude bounds: (lat_min, lat_max, lng_min, lng_max).
type Bounds = (f64, f64, f64, f64);

fn bounds_of(area: &[f64]) -> Bounds {
    let lats = area.iter().step_by(2);
    let lngs = area.iter().skip(1).step_by(2);
    (
        lats.clone().copied().fold(f64::INFINITY, f64::min),
        lats.copied().fold(f64::NEG_INFINITY, f64::max),
        lngs.clone().copied().fold(f64::INFINITY, f64::min),
        lngs.copied().fold(f64::NEG_INFINITY, f64::max),
    )
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct GeoIndex {
    /// Indexed by DocId.
    points: Vec<Vec<GeoPoint>>,
    cells: FxHashMap<(i32, i32), RoaringBitmap>,
    /// Records with at least one point.
    located: RoaringBitmap,
}

impl GeoIndex {
    pub fn add(&mut self, id: DocId, doc: &Map<String, Value>) {
        let points = points_of(doc);
        if points.is_empty() {
            return;
        }
        for p in &points {
            self.cells.entry(p.cell()).or_default().insert(id);
        }
        self.located.insert(id);
        if self.points.len() <= id as usize {
            self.points.resize_with(id as usize + 1, Vec::new);
        }
        self.points[id as usize] = points;
    }

    pub fn remove(&mut self, id: DocId) {
        let Some(points) = self.points.get_mut(id as usize) else { return };
        for p in std::mem::take(points) {
            if let Some(docs) = self.cells.get_mut(&p.cell()) {
                docs.remove(id);
                if docs.is_empty() {
                    self.cells.remove(&p.cell());
                }
            }
        }
        self.located.remove(id);
    }

    pub fn located(&self) -> &RoaringBitmap {
        &self.located
    }

    /// The record's nearest point to `center`, with its distance in meters.
    pub fn nearest(&self, doc: DocId, center: &GeoPoint) -> Option<(f64, GeoPoint)> {
        self.points
            .get(doc as usize)?
            .iter()
            .map(|p| (center.distance(p), *p))
            .min_by(|a, b| a.0.total_cmp(&b.0))
    }

    /// Records with a point in `bounds` that passes `keep`.
    fn in_bounds(&self, (lat_min, lat_max, lng_min, lng_max): Bounds, keep: impl Fn(&GeoPoint) -> bool) -> RoaringBitmap {
        let cell = |d: f64| (d / CELL_DEGREES).floor() as i64;
        let (lat_cells, lng_cells) = (cell(lat_min)..=cell(lat_max), cell(lng_min)..=cell(lng_max));
        let count = (lat_cells.end() - lat_cells.start() + 1) as usize * (lng_cells.end() - lng_cells.start() + 1) as usize;
        let mut near = RoaringBitmap::new();
        if count > MAX_CELLS.min(self.cells.len() * 4) {
            near = self.located.clone();
        } else {
            for lat in lat_cells {
                for lng in lng_cells.clone() {
                    if let Some(docs) = self.cells.get(&(lat as i32, lng_cell(lng))) {
                        near |= docs;
                    }
                }
            }
        }
        near.into_iter().filter(|&d| self.points[d as usize].iter().any(&keep)).collect()
    }

    /// Records within `radius` meters of `center`.
    pub fn within(&self, center: &GeoPoint, radius: f64) -> RoaringBitmap {
        let lat_delta = (radius / EARTH_RADIUS).to_degrees();
        let lng_delta = lat_delta / center.lat.to_radians().cos().max(1e-9);
        let keep = |p: &GeoPoint| center.distance(p) <= radius;
        let reaches_a_pole = center.lat.abs() + lat_delta >= 90.0;
        if reaches_a_pole || lng_delta >= 180.0 {
            return self.located.iter().filter(|&d| self.points[d as usize].iter().any(keep)).collect();
        }
        let (lat_min, lat_max) = (center.lat - lat_delta, center.lat + lat_delta);
        let (lng_min, lng_max) = (center.lng - lng_delta, center.lng + lng_delta);
        let mut docs = self.in_bounds((lat_min, lat_max, lng_min.max(-180.0), lng_max.min(180.0)), keep);
        // The circle may cross the 180th meridian: look on the other side too.
        if lng_min < -180.0 {
            docs |= self.in_bounds((lat_min, lat_max, lng_min + 360.0, 180.0), keep);
        }
        if lng_max > 180.0 {
            docs |= self.in_bounds((lat_min, lat_max, -180.0, lng_max - 360.0), keep);
        }
        docs
    }

    /// Records with a point inside any of the boxes `[lat1, lng1, lat2, lng2]`
    /// (two opposite corners).
    pub fn inside_boxes(&self, boxes: &[Vec<f64>]) -> RoaringBitmap {
        let mut docs = RoaringBitmap::new();
        for b in boxes {
            let bounds = bounds_of(b);
            let (lat_min, lat_max, lng_min, lng_max) = bounds;
            docs |= self.in_bounds(bounds, |p| {
                (lat_min..=lat_max).contains(&p.lat) && (lng_min..=lng_max).contains(&p.lng)
            });
        }
        docs
    }

    /// Records with a point inside any of the polygons.
    pub fn inside_polygons(&self, polygons: &[Vec<f64>]) -> RoaringBitmap {
        let mut docs = RoaringBitmap::new();
        for polygon in polygons {
            docs |= self.in_bounds(bounds_of(polygon), |p| in_polygon(p, polygon));
        }
        docs
    }

    /// The located `candidates` nearest to `center`, nearest first (ties by
    /// DocId), with their distances: the `k` nearest and every other one up
    /// to `reach(distance of the k-th)` (at least that distance), or all of
    /// them when there are no more than `k`.
    ///
    /// Searches rings of cells outward from the center and stops once
    /// nothing outside the searched square can be within reach; small sets
    /// are measured one by one.
    pub fn nearest_k(
        &self,
        center: &GeoPoint,
        candidates: &RoaringBitmap,
        k: usize,
        reach: impl Fn(f64) -> f64,
    ) -> Vec<(f64, DocId)> {
        let located = candidates & &self.located;
        let measure = |d: DocId| self.nearest(d, center).map(|(distance, _)| (distance, d));
        let sorted = |mut found: Vec<(f64, DocId)>| {
            found.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            found
        };
        let cut = |found: Vec<(f64, DocId)>| -> Vec<(f64, DocId)> {
            let found = sorted(found);
            match found.get(k.max(1) - 1) {
                Some(&(kth, _)) => {
                    let limit = reach(kth).max(kth);
                    found.into_iter().take_while(|&(d, _)| d <= limit).collect()
                }
                None => found,
            }
        };
        if located.len() <= SCAN_BELOW.max(k as u64) {
            return cut(located.iter().filter_map(measure).collect());
        }
        let (ci, cj) = ((center.lat / CELL_DEGREES).floor() as i64, (center.lng / CELL_DEGREES).floor() as i64);
        let mut found: Vec<(f64, DocId)> = Vec::new();
        let mut seen = RoaringBitmap::new();
        // Looking up a cell costs about as much as measuring a record or
        // two: past this many cells, measuring every record is cheaper.
        let budget = located.len() as usize;
        let mut visited = 0usize;
        for r in 0..MAX_RINGS {
            visited += if r == 0 { 1 } else { 8 * r as usize };
            if visited > budget {
                break;
            }
            for i in ci - r..=ci + r {
                let on_edge = i == ci - r || i == ci + r;
                let step = if on_edge { 1 } else { (2 * r).max(1) as usize };
                for j in (cj - r..=cj + r).step_by(step) {
                    let Some(docs) = self.cells.get(&(i as i32, lng_cell(j))) else { continue };
                    let mut fresh = docs & &located;
                    fresh -= &seen;
                    seen |= &fresh;
                    found.extend(fresh.iter().filter_map(measure));
                }
            }
            if found.len() < k {
                continue;
            }
            // Nearer than this, every record lies inside the searched square.
            let guaranteed = self.searched_reach(center, ci, cj, r);
            let mut distances: Vec<f64> = found.iter().map(|f| f.0).collect();
            let (_, &mut kth, _) = distances.select_nth_unstable_by(k.max(1) - 1, f64::total_cmp);
            if reach(kth).max(kth) <= guaranteed {
                return cut(found);
            }
        }
        cut(located.iter().filter_map(measure).collect())
    }

    /// A lower bound on the distance from `center` to any point outside
    /// the square of cells within `r` rings of cell (`ci`, `cj`).
    fn searched_reach(&self, center: &GeoPoint, ci: i64, cj: i64, r: i64) -> f64 {
        let (lat_lo, lat_hi) = ((ci - r) as f64 * CELL_DEGREES, (ci + r + 1) as f64 * CELL_DEGREES);
        let (lng_lo, lng_hi) = ((cj - r) as f64 * CELL_DEGREES, (cj + r + 1) as f64 * CELL_DEGREES);
        // Outside in latitude: at least the latitude gap.
        let below = if lat_lo <= -90.0 { f64::INFINITY } else { center.lat - lat_lo };
        let above = if lat_hi >= 90.0 { f64::INFINITY } else { lat_hi - center.lat };
        let by_lat = below.min(above).to_radians() * EARTH_RADIUS;
        // Outside in longitude only: the point's latitude is within the
        // square, so cos(latitude) >= cos(phi_max); haversine gives
        // d >= 2R asin(cos(phi_max) sin(dlng / 2)).
        let by_lng = if lng_hi - lng_lo >= 360.0 {
            f64::INFINITY
        } else {
            let dlng = (center.lng - lng_lo).min(lng_hi - center.lng).to_radians();
            let phi_max = lat_lo.abs().max(lat_hi.abs()).min(90.0).to_radians();
            2.0 * EARTH_RADIUS * (phi_max.cos() * (dlng / 2.0).sin()).clamp(0.0, 1.0).asin()
        };
        by_lat.min(by_lng)
    }

    /// The automatic radius: the distance of the `AUTOMATIC_RADIUS_HITS`-th
    /// nearest of `candidates` (the farthest when there are fewer), at least
    /// `minimum`; and the candidates within it.
    pub fn automatic_radius(&self, center: &GeoPoint, candidates: &RoaringBitmap, minimum: f64) -> (f64, RoaringBitmap) {
        let found = self.nearest_k(center, candidates, AUTOMATIC_RADIUS_HITS, |kth| kth.max(minimum));
        let farthest = found.last().map_or(0.0, |f| f.0);
        let radius = if found.len() >= AUTOMATIC_RADIUS_HITS { found[AUTOMATIC_RADIUS_HITS - 1].0 } else { farthest };
        let radius = radius.max(minimum);
        let within = if found.len() < AUTOMATIC_RADIUS_HITS && radius > farthest {
            // A minimum beyond every candidate found: all are within it.
            found.iter().map(|f| f.1).collect()
        } else {
            found.iter().filter(|f| f.0 <= radius).map(|f| f.1).collect()
        };
        (radius, within)
    }
}

/// The geo part of a search, read from its parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct GeoQuery {
    /// `aroundLatLng`: ranks by distance from it.
    pub center: Option<GeoPoint>,
    pub precision: f64,
    pub radius: Radius,
    pub minimum_radius: f64,
    pub boxes: Vec<Vec<f64>>,
    pub polygons: Vec<Vec<f64>>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Radius {
    Automatic,
    Meters(f64),
    All,
}

impl GeoQuery {
    pub fn is_empty(&self) -> bool {
        self.center.is_none() && self.boxes.is_empty() && self.polygons.is_empty()
    }

    /// The records the areas or the radius allow, unless the radius is
    /// automatic (applied later, with `automatic_radius`) or `"all"`.
    pub fn restriction(&self, geo: &GeoIndex) -> Option<RoaringBitmap> {
        if !self.boxes.is_empty() || !self.polygons.is_empty() {
            return Some(geo.inside_boxes(&self.boxes) | geo.inside_polygons(&self.polygons));
        }
        match (self.center, self.radius) {
            (Some(center), Radius::Meters(r)) => Some(geo.within(&center, r.max(self.minimum_radius))),
            _ => None,
        }
    }

    /// Whether an automatic radius applies.
    pub fn automatic(&self) -> Option<GeoPoint> {
        let areas = !self.boxes.is_empty() || !self.polygons.is_empty();
        self.center.filter(|_| self.radius == Radius::Automatic && !areas)
    }

    /// Ranking key of a distance: its step of `precision` meters.
    pub fn step(&self, distance: f64) -> u64 {
        (distance / self.precision.max(1.0)) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    fn ids(b: &RoaringBitmap) -> Vec<u32> {
        b.iter().collect()
    }

    const PARIS: GeoPoint = GeoPoint { lat: 48.8566, lng: 2.3522 };

    #[test]
    fn distances_are_great_circle_meters() {
        let london = GeoPoint { lat: 51.5074, lng: -0.1278 };
        // About 344 km.
        assert!((PARIS.distance(&london) - 343_500.0).abs() < 1_500.0, "{}", PARIS.distance(&london));
        assert_eq!(PARIS.distance(&PARIS), 0.0);
        let a = GeoPoint { lat: 0.0, lng: 179.9 };
        let b = GeoPoint { lat: 0.0, lng: -179.9 };
        assert!(a.distance(&b) < 23_000.0);
    }

    #[test]
    fn geoloc_formats_and_removal() {
        let mut g = GeoIndex::default();
        g.add(0, &doc(json!({"_geoloc": {"lat": 48.8566, "lng": 2.3522}})));
        g.add(1, &doc(json!({"_geoloc": [{"lat": "40.7128", "lng": "-74.0060"}, {"lat": 51.5, "lng": -0.12}]})));
        g.add(2, &doc(json!({"_geoloc": {"lat": 200, "lng": 0}})));
        g.add(3, &doc(json!({"name": "no location"})));
        assert_eq!(ids(g.located()), [0, 1]);
        // Record 1's nearest point to Paris is London.
        let (d, p) = g.nearest(1, &PARIS).unwrap();
        assert_eq!(p, GeoPoint { lat: 51.5, lng: -0.12 });
        assert!(d < 400_000.0);
        g.remove(1);
        assert_eq!(ids(g.located()), [0]);
        assert!(g.nearest(1, &PARIS).is_none());
        assert_eq!(ids(&g.within(&PARIS, 20_000_000.0)), [0]);
    }

    #[test]
    fn radius_boxes_and_polygons_agree_with_a_scan() {
        let mut g = GeoIndex::default();
        let mut seed = 5u64;
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 11) as f64 / (1u64 << 53) as f64
        };
        let mut points = Vec::new();
        for id in 0..3000u32 {
            // Clustered around Paris, plus some anywhere on Earth.
            let p = if id % 3 == 0 {
                GeoPoint { lat: next() * 180.0 - 90.0, lng: next() * 360.0 - 180.0 }
            } else {
                GeoPoint { lat: PARIS.lat + next() * 4.0 - 2.0, lng: PARIS.lng + next() * 6.0 - 3.0 }
            };
            g.add(id, &doc(json!({"_geoloc": {"lat": p.lat, "lng": p.lng}})));
            points.push(p);
        }
        let scan = |keep: &dyn Fn(&GeoPoint) -> bool| -> Vec<u32> {
            (0..3000u32).filter(|&i| keep(&points[i as usize])).collect()
        };
        for center in [PARIS, GeoPoint { lat: 0.0, lng: 179.5 }, GeoPoint { lat: 89.0, lng: 10.0 }] {
            for radius in [1_000.0, 50_000.0, 300_000.0, 3_000_000.0] {
                assert_eq!(ids(&g.within(&center, radius)), scan(&|p| center.distance(p) <= radius), "{center:?} {radius}");
            }
        }
        let boxes = vec![vec![49.0, 3.0, 48.5, 1.5], vec![-10.0, -10.0, 10.0, 10.0]];
        let in_box = |p: &GeoPoint, b: &[f64]| {
            p.lat >= b[0].min(b[2]) && p.lat <= b[0].max(b[2]) && p.lng >= b[1].min(b[3]) && p.lng <= b[1].max(b[3])
        };
        assert_eq!(ids(&g.inside_boxes(&boxes)), scan(&|p| boxes.iter().any(|b| in_box(p, b))));
        // A triangle around Paris.
        let triangle = vec![vec![50.0, 2.35, 48.0, 0.0, 48.0, 5.0]];
        let expected = scan(&|p| in_polygon(p, &triangle[0]));
        assert!(!expected.is_empty());
        assert_eq!(ids(&g.inside_polygons(&triangle)), expected);
    }

    #[test]
    fn automatic_radius_reaches_the_thousandth_nearest() {
        let mut g = GeoIndex::default();
        for id in 0..2000u32 {
            // One record every ~111 m going north from Paris.
            let lat = PARIS.lat + f64::from(id) * 0.001;
            g.add(id, &doc(json!({"_geoloc": {"lat": lat, "lng": PARIS.lng}})));
        }
        let all: RoaringBitmap = (0..2000).collect();
        let (radius, within) = g.automatic_radius(&PARIS, &all, 0.0);
        assert_eq!(g.within(&PARIS, radius).len(), 1000);
        assert_eq!(within, g.within(&PARIS, radius));
        // Fewer candidates: wide enough for all of them.
        let few: RoaringBitmap = (0..10).collect();
        let (radius, within) = g.automatic_radius(&PARIS, &few, 0.0);
        assert!(radius < 1_100.0 && within == few);
        let (radius, within) = g.automatic_radius(&PARIS, &few, 50_000.0);
        assert!(radius == 50_000.0 && within == few);
    }

    #[test]
    fn nearest_k_agrees_with_measuring_everything() {
        let mut g = GeoIndex::default();
        let mut seed = 9u64;
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 11) as f64 / (1u64 << 53) as f64
        };
        let centers = [PARIS, GeoPoint { lat: 0.0, lng: 179.9 }, GeoPoint { lat: -60.0, lng: -170.0 }, GeoPoint { lat: 85.0, lng: 0.0 }];
        for id in 0..20_000u32 {
            // Dense clusters around each center, and a sparse rest.
            let c = centers[id as usize % 4];
            let p = if id % 5 == 0 {
                GeoPoint { lat: next() * 170.0 - 85.0, lng: next() * 360.0 - 180.0 }
            } else {
                let lng = c.lng + next() * 8.0 - 4.0;
                let lng = if lng > 180.0 { lng - 360.0 } else if lng < -180.0 { lng + 360.0 } else { lng };
                GeoPoint { lat: (c.lat + next() * 4.0 - 2.0).clamp(-90.0, 90.0), lng }
            };
            g.add(id, &doc(json!({"_geoloc": {"lat": p.lat, "lng": p.lng}})));
        }
        let some: RoaringBitmap = (0..20_000u32).filter(|i| i % 3 != 0).collect();
        for center in centers.iter().chain(&[GeoPoint { lat: 30.0, lng: 60.0 }]) {
            let mut all: Vec<(f64, DocId)> = some.iter().filter_map(|d| g.nearest(d, center).map(|(x, _)| (x, d))).collect();
            all.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            for k in [1, 20, 1000, 5000] {
                for reach in [0.0, 50_000.0] {
                    let found = g.nearest_k(center, &some, k, |kth| kth + reach);
                    let limit = all[k - 1].0 + reach;
                    let expected: Vec<(f64, DocId)> = all.iter().copied().take_while(|f| f.0 <= limit).collect();
                    assert_eq!(found, expected, "{center:?} k {k} reach {reach}");
                }
            }
        }
    }

    #[test]
    fn parameters_parse_in_algolia_forms() {
        assert_eq!(parse_point("48.8566, 2.3522"), Ok(PARIS));
        assert!(parse_point("48.8566").is_err() && parse_point("91, 0").is_err() && parse_point("a, b").is_err());
        let expected = vec![vec![1.0, 2.0, 3.0, 4.0]];
        assert_eq!(parse_areas(&json!([[1, 2, 3, 4]]), "box", 4, true), Ok(expected.clone()));
        assert_eq!(parse_areas(&json!([1, 2, 3, 4]), "box", 4, true), Ok(expected.clone()));
        assert_eq!(parse_areas(&json!("1,2,3,4"), "box", 4, true), Ok(expected.clone()));
        assert_eq!(parse_areas(&json!("[[1,2,3,4]]"), "box", 4, true), Ok(expected));
        assert!(parse_areas(&json!([1, 2, 3]), "box", 4, true).is_err());
        assert!(parse_areas(&json!([[1, 2, 3, 4, 5, 6]]), "polygon", 6, false).is_ok());
        assert!(parse_areas(&json!([[1, 2, 3, 4, 5]]), "polygon", 6, false).is_err());
    }
}
