use crate::error::AppError;
use base64::{engine::general_purpose::STANDARD, Engine};
use flate2::read::DeflateDecoder;
use roxmltree::{Document, Node};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    io::Read,
};
const MAX_XML: usize = 2 * 1024 * 1024;
fn invalid(s: impl Into<String>) -> AppError {
    AppError::Validation(s.into())
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImportedNode {
    pub cell_id: String,
    pub label: String,
    pub kind: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub ip: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Edge {
    pub source: String,
    pub target: String,
    pub label: String,
    pub points: Vec<[f64; 2]>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Diagram {
    pub name: String,
    pub nodes: Vec<ImportedNode>,
    pub edges: Vec<Edge>,
}

pub fn pages(xml: &str) -> Result<Vec<(String, String)>, AppError> {
    if xml.len() > MAX_XML {
        return Err(invalid("Diagram exceeds the 2 MiB limit"));
    }
    let doc = Document::parse(xml).map_err(|e| invalid(format!("Invalid draw.io XML: {e}")))?;
    let root = doc.root_element();
    if root.tag_name().name() == "mxGraphModel" {
        return Ok(vec![("Topology".into(), xml.into())]);
    }
    if root.tag_name().name() != "mxfile" {
        return Err(invalid(
            "Upload a .drawio or XML file containing mxfile or mxGraphModel",
        ));
    }
    let mut output = Vec::new();
    for diagram in root.children().filter(|n| n.has_tag_name("diagram")) {
        if output.len() >= 20 {
            return Err(invalid("Import at most 20 pages at once"));
        }
        let name = diagram.attribute("name").unwrap_or("Topology").to_owned();
        let graph = if let Some(graph) = diagram.children().find(|n| n.has_tag_name("mxGraphModel"))
        {
            xml[graph.range()].to_owned()
        } else {
            let encoded = diagram.text().unwrap_or("").trim();
            let bytes = STANDARD
                .decode(encoded)
                .map_err(|_| invalid("Invalid compressed draw.io page (base64)"))?;
            let mut inflated = String::new();
            DeflateDecoder::new(&bytes[..])
                .take((MAX_XML + 1) as u64)
                .read_to_string(&mut inflated)
                .map_err(|_| invalid("Unable to decompress draw.io page"))?;
            if inflated.len() > MAX_XML {
                return Err(invalid("Expanded diagram exceeds 2 MiB"));
            }
            percent_encoding::percent_decode_str(&inflated)
                .decode_utf8()
                .map_err(|_| invalid("Diagram is not valid UTF-8"))?
                .into_owned()
        };
        output.push((name, graph));
    }
    if output.is_empty() {
        return Err(invalid("This file contains no diagram pages"));
    }
    Ok(output)
}
fn number(n: Node<'_, '_>, name: &str, default: f64) -> f64 {
    n.attribute(name)
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| v.is_finite())
        .unwrap_or(default)
        .clamp(-100000.0, 100000.0)
}
fn label(value: &str) -> String {
    let value = value
        .replace("<br>", "\n")
        .replace("<br/>", "\n")
        .replace("<br />", "\n");
    let mut inside = false;
    value
        .chars()
        .filter(|c| {
            if *c == '<' {
                inside = true;
                return false;
            }
            if *c == '>' {
                inside = false;
                return false;
            }
            !inside
        })
        .collect::<String>()
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .chars()
        .take(200)
        .collect::<String>()
        .trim()
        .into()
}
fn kind(style: &str, label: &str) -> String {
    let value = format!("{style} {label}").to_lowercase();
    if value.contains("swimlane")
        || style
            .split(';')
            .any(|part| part == "group" || part == "shape=group")
    {
        "group"
    } else if value.contains("router") {
        "router"
    } else if value.contains("switch") {
        "switch"
    } else if value.contains("server") || value.contains("rack") {
        "server"
    } else if value.contains("firewall") {
        "firewall"
    } else if value.contains("cloud") || value.contains("internet") {
        "cloud"
    } else if value.contains("text;") || value.contains("shape=label") {
        "label"
    } else {
        "host"
    }
    .into()
}
pub fn parse(name: &str, xml: &str) -> Result<Diagram, AppError> {
    let doc = Document::parse(xml).map_err(|e| invalid(format!("Invalid page XML: {e}")))?;
    let cells: Vec<_> = doc
        .descendants()
        .filter(|n| n.has_tag_name("mxCell"))
        .collect();
    if cells.len() > 5000 {
        return Err(invalid("Diagram has too many objects (maximum 5000)"));
    }
    let mut seen = HashSet::new();
    let by_id: HashMap<String, Node<'_, '_>> = cells
        .iter()
        .filter_map(|cell| {
            let id = cell
                .attribute("id")
                .or_else(|| cell.parent().and_then(|p| p.attribute("id")))?;
            Some((id.into(), *cell))
        })
        .collect();
    let mut nodes = Vec::new();
    for cell in &cells {
        if cell.attribute("vertex") != Some("1") {
            continue;
        }
        let id = cell
            .attribute("id")
            .or_else(|| cell.parent().and_then(|p| p.attribute("id")))
            .unwrap_or("");
        if id.is_empty() || id.len() > 200 || !seen.insert(id.to_owned()) {
            return Err(invalid("Diagram has missing or duplicate object IDs"));
        }
        let Some(geo) = cell.children().find(|n| n.has_tag_name("mxGeometry")) else {
            continue;
        };
        let mut x = number(geo, "x", 0.0);
        let mut y = number(geo, "y", 0.0);
        let mut parent = cell.attribute("parent").unwrap_or("1");
        let mut parents = HashSet::new();
        while let Some(ancestor) = by_id.get(parent) {
            if !parents.insert(parent.to_owned()) {
                return Err(invalid("Diagram contains a parent cycle"));
            }
            if let Some(g) = ancestor.children().find(|n| n.has_tag_name("mxGeometry")) {
                if geo.attribute("relative") == Some("1") && parents.len() == 1 {
                    x *= number(g, "width", 1.0);
                    y *= number(g, "height", 1.0);
                }
                x += number(g, "x", 0.0);
                y += number(g, "y", 0.0);
            }
            parent = ancestor.attribute("parent").unwrap_or("");
        }
        let wrapper = cell.parent();
        let value = cell
            .attribute("value")
            .or_else(|| wrapper.and_then(|p| p.attribute("label")))
            .unwrap_or("");
        let label = label(value);
        let ip = wrapper
            .and_then(|p| p.attribute("ip").or_else(|| p.attribute("ip_address")))
            .or_else(|| cell.attribute("ip"))
            .filter(|ip| {
                ip.parse::<std::net::IpAddr>().is_ok_and(|ip| {
                    !ip.is_multicast()
                        && !ip.is_unspecified()
                        && ip != std::net::IpAddr::V4(std::net::Ipv4Addr::BROADCAST)
                })
            })
            .unwrap_or("")
            .into();
        nodes.push(ImportedNode {
            cell_id: id.into(),
            kind: kind(cell.attribute("style").unwrap_or(""), &label),
            label,
            x,
            y,
            width: number(geo, "width", 80.0).clamp(16.0, 3000.0),
            height: number(geo, "height", 60.0).clamp(16.0, 3000.0),
            ip,
        });
    }
    if nodes.is_empty() {
        return Err(invalid("No drawable devices were found on this page"));
    }
    if nodes.len() > 1000 {
        return Err(invalid("Maximum 1000 devices per diagram"));
    }
    let ids: HashSet<_> = nodes.iter().map(|n| n.cell_id.as_str()).collect();
    let mut edges = Vec::new();
    for cell in &cells {
        if cell.attribute("edge") != Some("1") {
            continue;
        }
        let source = cell.attribute("source").unwrap_or("");
        let target = cell.attribute("target").unwrap_or("");
        if !ids.contains(source) || !ids.contains(target) {
            continue;
        }
        let points = cell
            .descendants()
            .filter(|n| {
                n.has_tag_name("mxPoint") && n.parent().is_some_and(|p| p.has_tag_name("Array"))
            })
            .map(|n| [number(n, "x", 0.0), number(n, "y", 0.0)])
            .collect();
        edges.push(Edge {
            source: source.into(),
            target: target.into(),
            label: label(cell.attribute("value").unwrap_or("")),
            points,
        });
    }
    Ok(Diagram {
        name: name.chars().take(100).collect(),
        nodes,
        edges,
    })
}
