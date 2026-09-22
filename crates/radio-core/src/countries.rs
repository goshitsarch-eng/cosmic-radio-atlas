//! Country outlines from public-domain Natural Earth data.
//!
//! The embedded `countries.json` is the same asset the legacy QML globe used.
//! Geometry is normalized so `Polygon` and `MultiPolygon` features share one
//! shape: a list of polygons, each a list of linear rings.

use std::sync::LazyLock;

use serde::Deserialize;

use crate::error::{Error, Result};

/// One linear ring: `[longitude, latitude]` pairs.
pub type Ring = Vec<[f64; 2]>;
/// One polygon: an outer ring plus optional holes.
pub type Polygon = Vec<Ring>;

#[derive(Debug, Clone, Default)]
pub struct Properties {
    pub name: String,
    pub code: String,
}

#[derive(Debug, Clone, Default)]
pub struct Feature {
    pub properties: Properties,
    pub geometry: Geometry,
}

#[derive(Debug, Clone, Default)]
pub struct Geometry {
    polygons: Vec<Polygon>,
}

impl Geometry {
    pub fn polygons(&self) -> &[Polygon] {
        &self.polygons
    }

    pub fn is_empty(&self) -> bool {
        self.polygons.is_empty()
    }
}

#[derive(Debug, Deserialize)]
struct RawCollection {
    #[serde(default)]
    features: Vec<RawFeature>,
}

#[derive(Debug, Deserialize)]
struct RawFeature {
    #[serde(default)]
    properties: RawProperties,
    geometry: Option<RawGeometry>,
}

#[derive(Debug, Default, Deserialize)]
struct RawProperties {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    code: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum RawGeometry {
    Polygon {
        coordinates: Vec<Vec<Vec<f64>>>,
    },
    MultiPolygon {
        coordinates: Vec<Vec<Vec<Vec<f64>>>>,
    },
}

fn convert_ring(raw: &[Vec<f64>]) -> Option<Ring> {
    let mut ring = Ring::with_capacity(raw.len());
    for point in raw {
        let (Some(&lon), Some(&lat)) = (point.first(), point.get(1)) else {
            continue;
        };
        if !lon.is_finite() || !lat.is_finite() {
            continue;
        }
        ring.push([lon, lat]);
    }
    if ring.len() < 3 { None } else { Some(ring) }
}

/// Parse a GeoJSON feature collection into normalized features.
pub fn load_countries(json: &str) -> Result<Vec<Feature>> {
    let collection: RawCollection = serde_json::from_str(json).map_err(Error::Json)?;
    let mut features = Vec::with_capacity(collection.features.len());
    for raw in collection.features {
        let Some(geometry) = raw.geometry else {
            continue;
        };
        let raw_polygons: Vec<Vec<Vec<Vec<f64>>>> = match geometry {
            RawGeometry::Polygon { coordinates } => vec![coordinates],
            RawGeometry::MultiPolygon { coordinates } => coordinates,
        };
        let mut polygons = Vec::new();
        for raw_polygon in &raw_polygons {
            let mut polygon = Polygon::new();
            for raw_ring in raw_polygon {
                if let Some(ring) = convert_ring(raw_ring) {
                    polygon.push(ring);
                }
            }
            if !polygon.is_empty() {
                polygons.push(polygon);
            }
        }
        if polygons.is_empty() {
            continue;
        }
        features.push(Feature {
            properties: Properties {
                name: raw.properties.name.unwrap_or_default(),
                code: raw.properties.code.unwrap_or_default(),
            },
            geometry: Geometry { polygons },
        });
    }
    Ok(features)
}

static EMBEDDED: LazyLock<Vec<Feature>> =
    LazyLock::new(|| load_countries(include_str!("../assets/countries.json")).unwrap_or_default());

/// The embedded Natural Earth country outlines.
pub fn embedded_countries() -> &'static [Feature] {
    &EMBEDDED
}

/// Look up a country's display name by ISO code.
pub fn country_name<'a>(features: &'a [Feature], code: &str) -> Option<&'a str> {
    let wanted = code.to_ascii_uppercase();
    features
        .iter()
        .find(|feature| feature.properties.code.to_ascii_uppercase() == wanted)
        .map(|feature| feature.properties.name.as_str())
        .filter(|name| !name.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_embedded_countries() {
        let features = embedded_countries();
        assert!(features.len() > 150);
        assert_eq!(
            country_name(features, "us"),
            Some("United States of America")
        );
        assert!(features.iter().all(|f| !f.geometry.is_empty()));
    }

    #[test]
    fn skips_invalid_features() {
        let json = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","properties":{"name":"Nowhere","code":"XX"},"geometry":null},
            {"type":"Feature","properties":{"name":"Tiny","code":"TT"},
             "geometry":{"type":"Polygon","coordinates":[[[0,0],[1,1]]]}},
            {"type":"Feature","properties":{"name":"Square","code":"SQ"},
             "geometry":{"type":"Polygon","coordinates":[[[0,0],[1,0],[1,1],[0,0]]]}},
            {"type":"Feature","properties":{"name":"Multi","code":"MM"},
             "geometry":{"type":"MultiPolygon","coordinates":[[[[0,0],[1,0],[1,1],[0,0]]]]}}
        ]}"#;
        let features = load_countries(json).unwrap();
        assert_eq!(features.len(), 2);
    }
}
