// SPDX-License-Identifier: GPL-3.0-or-later
//! Applying board edit plans (Phase 12) to a live board: the outline, mounting holes, and
//! footprints moved, turned, flipped, and locked, all in one commit (one undo step in the editor).
//!
//! KiCad 10.99 moves only a footprint's anchor when `UpdateItems` changes its position or
//! orientation: the pads, drawings, and text inside it are sent back with their old places on the
//! board, and KiCad keeps those. A footprint sent without them loses them. So a move turns and
//! shifts every item of the footprint here first (the Phase 12 probe), and the read-back checks
//! every pad.

use std::time::{Duration, Instant};

use kicad_domain::board::place_point;
use kicad_domain::board_edit::{BoardEditPlan, BoardOp};
use kicad_domain::{ErrorCode, MutationState, Nm, Point, Rect, ToolError};
use kicad_ipc::proto::kiapi::board::commands::{BoardFlipDirection, FlipItems, FlipItemsResponse};
use kicad_ipc::proto::kiapi::board::types::{
    BoardGraphicShape, BoardLayer, BoardText, BoardTextBox, Field, FieldId, Footprint, FootprintInstance, Pad, Zone,
};
use kicad_ipc::proto::kiapi::common::commands::{
    BeginCommit, BeginCommitResponse, CommitAction, CreateItems, CreateItemsResponse, DeleteItems, EndCommit, GetItems,
    GetItemsFromLibrary, GetItemsResponse, ItemDeletionStatus, ItemStatusCode, LoadAllLibraries, UpdateItems,
    UpdateItemsResponse,
};
use kicad_ipc::proto::kiapi::common::types::{
    graphic_shape, poly_line_node, Angle, Distance, GraphicAttributes, GraphicRectangleAttributes, GraphicShape,
    ItemHeader, KiCadObjectType, Kiid as ProtoKiid, LibraryIdentifier, LibraryType, LockedState, PolyLine,
    PolyLineNode, PolySet, StrokeAttributes, TextAttributes, Vector2,
};
use kicad_ipc::Client;
use prost::Name;
use prost_types::Any;
use serde::Serialize;

use super::BoardSession;
use crate::errors::from_ipc_error;
use crate::session::Provider;

/// How long a headless server may still be loading footprint libraries before "not found" is final.
const LIBRARY_LOAD_GRACE: Duration = Duration::from_secs(90);

/// What applying a board edit did.
#[derive(Debug, Clone, Default, Serialize)]
pub struct EditReport {
    pub removed_edges: usize,
    pub outline_added: bool,
    /// Footprints placed from the library, by reference.
    pub placed: Vec<String>,
    pub moved: Vec<String>,
    pub flipped: Vec<String>,
}

fn kerr(message: impl Into<String>) -> ToolError {
    ToolError::new(ErrorCode::KicadError, message)
}

fn vector(p: Point) -> Option<Vector2> {
    Some(Vector2 {
        x_nm: p.x.0,
        y_nm: p.y.0,
    })
}

fn point(v: &Vector2) -> Point {
    Point::new(Nm(v.x_nm), Nm(v.y_nm))
}

/// The kind and bounding box of a board drawing, for the outline list.
pub(super) fn shape_bounds(shape: &GraphicShape) -> (&'static str, Option<Rect>) {
    let pts = |list: &[&Option<Vector2>]| Rect::from_points(list.iter().filter_map(|v| v.as_ref()).map(point));
    match &shape.geometry {
        Some(graphic_shape::Geometry::Segment(s)) => ("segment", pts(&[&s.start, &s.end])),
        Some(graphic_shape::Geometry::Rectangle(r)) => ("rectangle", pts(&[&r.top_left, &r.bottom_right])),
        Some(graphic_shape::Geometry::Arc(a)) => ("arc", pts(&[&a.start, &a.mid, &a.end])),
        Some(graphic_shape::Geometry::Circle(c)) => {
            let (Some(centre), Some(edge)) = (&c.center, &c.radius_point) else {
                return ("circle", None);
            };
            let r = ((edge.x_nm - centre.x_nm) as f64)
                .hypot((edge.y_nm - centre.y_nm) as f64)
                .round() as i64;
            let c = point(centre);
            (
                "circle",
                Rect::from_points([c - Point::new(Nm(r), Nm(r)), c + Point::new(Nm(r), Nm(r))]),
            )
        }
        Some(graphic_shape::Geometry::Polygon(p)) => (
            "polygon",
            Rect::from_points(p.polygons.iter().flat_map(|poly| {
                poly.outline
                    .iter()
                    .flat_map(|l| l.nodes.iter())
                    .flat_map(|n| match &n.geometry {
                        Some(poly_line_node::Geometry::Point(v)) => vec![point(v)],
                        Some(poly_line_node::Geometry::Arc(a)) => [&a.start, &a.mid, &a.end]
                            .iter()
                            .filter_map(|v| v.as_ref())
                            .map(point)
                            .collect(),
                        None => vec![],
                    })
            })),
        ),
        _ => ("other", None),
    }
}

/// A rigid move of everything in a footprint: from the anchor `from` to `to`, turned by `deg`
/// (counter-clockwise on screen) about the anchor.
struct Rigid {
    from: Point,
    to: Point,
    deg: f64,
}

impl Rigid {
    fn right_angle(&self) -> bool {
        let r = self.deg.rem_euclid(90.0);
        r < 1e-9 || 90.0 - r < 1e-9
    }

    fn point(&self, v: &mut Option<Vector2>) {
        if let Some(v) = v.as_mut() {
            let q = place_point(self.to, self.deg, point(v) - self.from);
            v.x_nm = q.x.0;
            v.y_nm = q.y.0;
        }
    }

    fn angle(&self, a: &mut Option<Angle>) {
        if self.deg == 0.0 {
            return;
        }
        let a = a.get_or_insert_with(Angle::default);
        a.value_degrees = kicad_domain::board_edit::normalize_deg(a.value_degrees + self.deg);
    }

    fn text(&self, t: &mut Option<kicad_ipc::proto::kiapi::common::types::Text>) {
        if let Some(t) = t.as_mut() {
            self.point(&mut t.position);
            if let Some(attrs) = t.attributes.as_mut() {
                self.angle(&mut attrs.angle);
            }
        }
    }

    fn board_text(&self, t: &mut Option<BoardText>) {
        if let Some(t) = t.as_mut() {
            self.text(&mut t.text);
        }
    }

    fn field(&self, f: &mut Option<Field>) {
        if let Some(f) = f.as_mut() {
            self.board_text(&mut f.text);
        }
    }

    fn line(&self, l: &mut PolyLine) {
        for n in &mut l.nodes {
            match &mut n.geometry {
                Some(poly_line_node::Geometry::Point(v)) => {
                    let mut o = Some(*v);
                    self.point(&mut o);
                    *v = o.expect("set");
                }
                Some(poly_line_node::Geometry::Arc(a)) => {
                    self.point(&mut a.start);
                    self.point(&mut a.mid);
                    self.point(&mut a.end);
                }
                None => {}
            }
        }
    }

    fn poly(&self, p: &mut PolySet) {
        for poly in &mut p.polygons {
            if let Some(o) = poly.outline.as_mut() {
                self.line(o);
            }
            for h in &mut poly.holes {
                self.line(h);
            }
        }
    }

    fn shape(&self, s: &mut GraphicShape) {
        match &mut s.geometry {
            Some(graphic_shape::Geometry::Segment(g)) => {
                self.point(&mut g.start);
                self.point(&mut g.end);
            }
            Some(graphic_shape::Geometry::Rectangle(g)) => {
                if self.right_angle() {
                    self.point(&mut g.top_left);
                    self.point(&mut g.bottom_right);
                    // Keep the corners in order after a turn.
                    if let (Some(a), Some(b)) = (g.top_left.as_mut(), g.bottom_right.as_mut()) {
                        let (x0, x1) = (a.x_nm.min(b.x_nm), a.x_nm.max(b.x_nm));
                        let (y0, y1) = (a.y_nm.min(b.y_nm), a.y_nm.max(b.y_nm));
                        (a.x_nm, a.y_nm, b.x_nm, b.y_nm) = (x0, y0, x1, y1);
                    }
                } else {
                    // A rectangle turned by another angle is a polygon, as in KiCad.
                    let (Some(a), Some(b)) = (g.top_left, g.bottom_right) else {
                        return;
                    };
                    let corners = [(a.x_nm, a.y_nm), (b.x_nm, a.y_nm), (b.x_nm, b.y_nm), (a.x_nm, b.y_nm)];
                    let mut line = PolyLine {
                        nodes: corners
                            .iter()
                            .map(|(x, y)| PolyLineNode {
                                geometry: Some(poly_line_node::Geometry::Point(Vector2 { x_nm: *x, y_nm: *y })),
                            })
                            .collect(),
                        closed: true,
                    };
                    self.line(&mut line);
                    s.geometry = Some(graphic_shape::Geometry::Polygon(PolySet {
                        polygons: vec![kicad_ipc::proto::kiapi::common::types::PolygonWithHoles {
                            outline: Some(line),
                            holes: vec![],
                        }],
                    }));
                }
            }
            Some(graphic_shape::Geometry::Arc(g)) => {
                self.point(&mut g.start);
                self.point(&mut g.mid);
                self.point(&mut g.end);
            }
            Some(graphic_shape::Geometry::Circle(g)) => {
                self.point(&mut g.center);
                self.point(&mut g.radius_point);
            }
            Some(graphic_shape::Geometry::Polygon(g)) => self.poly(g),
            Some(graphic_shape::Geometry::Bezier(g)) => {
                self.point(&mut g.start);
                self.point(&mut g.control1);
                self.point(&mut g.control2);
                self.point(&mut g.end);
            }
            Some(graphic_shape::Geometry::Ellipse(g)) => {
                self.point(&mut g.center);
                self.angle(&mut g.rotation);
            }
            Some(graphic_shape::Geometry::EllipseArc(g)) => {
                self.point(&mut g.center);
                self.angle(&mut g.rotation);
            }
            None => {}
        }
    }

    /// Moves the footprint and everything in it. Refuses a footprint holding an item it cannot
    /// move, rather than leave that item behind.
    fn footprint(&self, f: &mut FootprintInstance, reference: &str) -> Result<(), ToolError> {
        f.position = vector(self.to);
        self.angle(&mut f.orientation);
        for field in [
            &mut f.reference_field,
            &mut f.value_field,
            &mut f.datasheet_field,
            &mut f.description_field,
        ] {
            self.field(field);
        }
        let Some(d) = f.definition.as_mut() else {
            return Ok(());
        };
        for field in [
            &mut d.reference_field,
            &mut d.value_field,
            &mut d.datasheet_field,
            &mut d.description_field,
        ] {
            self.field(field);
        }
        let encode = |e: prost::EncodeError| kerr(e.to_string());
        let decode = |e: prost::DecodeError| kerr(e.to_string());
        for item in &mut d.items {
            let url = item.type_url.as_str();
            if url == Pad::type_url() {
                let mut p: Pad = item.to_msg().map_err(decode)?;
                self.point(&mut p.position);
                if let Some(stack) = p.pad_stack.as_mut() {
                    self.angle(&mut stack.angle);
                }
                *item = Any::from_msg(&p).map_err(encode)?;
            } else if url == BoardGraphicShape::type_url() {
                let mut s: BoardGraphicShape = item.to_msg().map_err(decode)?;
                if let Some(g) = s.shape.as_mut() {
                    self.shape(g);
                }
                *item = Any::from_msg(&s).map_err(encode)?;
            } else if url == BoardText::type_url() {
                let mut t: BoardText = item.to_msg().map_err(decode)?;
                self.text(&mut t.text);
                *item = Any::from_msg(&t).map_err(encode)?;
            } else if url == Field::type_url() {
                let mut t: Field = item.to_msg().map_err(decode)?;
                self.board_text(&mut t.text);
                *item = Any::from_msg(&t).map_err(encode)?;
            } else if url == BoardTextBox::type_url() {
                if !self.right_angle() {
                    return Err(ToolError::new(
                        ErrorCode::InvalidArgument,
                        format!("{reference} has a text box, which can only be turned by a multiple of 90°"),
                    ));
                }
                let mut t: BoardTextBox = item.to_msg().map_err(decode)?;
                if let Some(b) = t.textbox.as_mut() {
                    self.point(&mut b.top_left);
                    self.point(&mut b.bottom_right);
                    if let (Some(a), Some(c)) = (b.top_left.as_mut(), b.bottom_right.as_mut()) {
                        let (x0, x1) = (a.x_nm.min(c.x_nm), a.x_nm.max(c.x_nm));
                        let (y0, y1) = (a.y_nm.min(c.y_nm), a.y_nm.max(c.y_nm));
                        (a.x_nm, a.y_nm, c.x_nm, c.y_nm) = (x0, y0, x1, y1);
                    }
                    if let Some(attrs) = b.attributes.as_mut() {
                        self.angle(&mut attrs.angle);
                    }
                }
                *item = Any::from_msg(&t).map_err(encode)?;
            } else if url == Zone::type_url() {
                let mut z: Zone = item.to_msg().map_err(decode)?;
                if let Some(o) = z.outline.as_mut() {
                    self.poly(o);
                }
                for fill in &mut z.filled_polygons {
                    if let Some(s) = fill.shapes.as_mut() {
                        self.poly(s);
                    }
                }
                *item = Any::from_msg(&z).map_err(encode)?;
            } else if url.ends_with(".Footprint3DModel") {
                // Its offset and turn are the footprint's own, so it moves with the anchor.
            } else {
                return Err(ToolError::new(
                    ErrorCode::FeatureUnavailable,
                    format!(
                        "{reference} holds an item this tool cannot move ({}); move it in KiCad",
                        url.rsplit('.').next().unwrap_or(url)
                    ),
                ));
            }
        }
        Ok(())
    }
}

fn status_ok(status: &Option<kicad_ipc::proto::kiapi::common::commands::ItemStatus>) -> Result<(), String> {
    match status {
        Some(s) if s.code == ItemStatusCode::IscOk as i32 => Ok(()),
        Some(s) => Err(s.error_message.clone()),
        None => Err("no status".into()),
    }
}

/// A footprint instance at the origin, front side, from a library definition, with its reference
/// on the silkscreen above the courtyard and its value on the fabrication layer below (the
/// library's own field places are not in the definition KiCad returns).
fn new_instance(definition: Footprint, reference: &str, value: &str, reference_below: bool) -> FootprintInstance {
    let courtyard = Rect::from_points(
        definition
            .items
            .iter()
            .filter(|i| i.type_url == BoardGraphicShape::type_url())
            .filter_map(|i| i.to_msg::<BoardGraphicShape>().ok())
            .filter(|s| s.layer == BoardLayer::BlFCrtYd as i32)
            .filter_map(|s| s.shape.as_ref().and_then(|g| shape_bounds(g).1))
            .flat_map(|r| [r.min, r.max]),
    )
    .unwrap_or(Rect {
        min: Point::new(Nm(-1_000_000), Nm(-1_000_000)),
        max: Point::new(Nm(1_000_000), Nm(1_000_000)),
    });
    let (above, below) = (courtyard.min.y - Nm(1_000_000), courtyard.max.y + Nm(1_000_000));
    let field = |id: i32, name: &str, text: &str, y: Nm, layer: BoardLayer| {
        Some(Field {
            id: Some(FieldId { id }),
            name: name.into(),
            text: Some(BoardText {
                text: Some(kicad_ipc::proto::kiapi::common::types::Text {
                    position: vector(Point::new(Nm(0), y)),
                    attributes: Some(TextAttributes {
                        size: Some(Vector2 {
                            x_nm: 1_000_000,
                            y_nm: 1_000_000,
                        }),
                        stroke_width: Some(Distance { value_nm: 150_000 }),
                        visible: true,
                        angle: Some(Angle { value_degrees: 0.0 }),
                        ..Default::default()
                    }),
                    text: text.into(),
                    ..Default::default()
                }),
                layer: layer as i32,
                ..Default::default()
            }),
            visible: true,
            ..Default::default()
        })
    };
    FootprintInstance {
        position: vector(Point::default()),
        orientation: Some(Angle { value_degrees: 0.0 }),
        layer: BoardLayer::BlFCu as i32,
        locked: LockedState::LsUnlocked as i32,
        reference_field: field(
            0,
            "Reference",
            reference,
            if reference_below { below } else { above },
            BoardLayer::BlFSilkS,
        ),
        value_field: field(
            1,
            "Value",
            value,
            if reference_below { above } else { below },
            BoardLayer::BlFFab,
        ),
        definition: Some(definition),
        ..Default::default()
    }
}

async fn footprints(
    client: &mut Client,
    header: &impl Fn() -> Option<ItemHeader>,
) -> Result<Vec<FootprintInstance>, ToolError> {
    let r: GetItemsResponse = client
        .call(&GetItems {
            header: header(),
            types: vec![KiCadObjectType::KotPcbFootprint as i32],
        })
        .await
        .map_err(|e| from_ipc_error(&e, false))?;
    r.items
        .iter()
        .filter(|a| a.type_url == FootprintInstance::type_url())
        .map(|a| a.to_msg::<FootprintInstance>().map_err(|e| kerr(e.to_string())))
        .collect()
}

async fn update(
    client: &mut Client,
    header: &impl Fn() -> Option<ItemHeader>,
    items: Vec<FootprintInstance>,
) -> Result<(), ToolError> {
    if items.is_empty() {
        return Ok(());
    }
    let anys = items
        .iter()
        .map(|f| Any::from_msg(f).map_err(|e| kerr(e.to_string())))
        .collect::<Result<Vec<_>, _>>()?;
    let r: UpdateItemsResponse = client
        .call(&UpdateItems {
            header: header(),
            items: anys,
        })
        .await
        .map_err(|e| from_ipc_error(&e, true))?;
    let failed: Vec<String> = r
        .updated_items
        .iter()
        .filter_map(|u| status_ok(&u.status).err())
        .collect();
    if !failed.is_empty() || r.updated_items.len() != items.len() {
        return Err(kerr(format!(
            "KiCad updated {} of {} footprints: {}",
            r.updated_items.len() - failed.len(),
            items.len(),
            failed.join("; ")
        )));
    }
    Ok(())
}

impl BoardSession {
    /// Applies a board edit plan in one commit. On a failure the commit is dropped, so nothing
    /// stays changed; the error says whether that worked.
    pub async fn apply_edit(&self, plan: &BoardEditPlan) -> Result<EditReport, ToolError> {
        let ipc = self.ipc.as_ref().ok_or_else(|| {
            ToolError::new(
                ErrorCode::FeatureUnavailable,
                "editing a board needs a live KiCad 11 session (headless or gui); a file session only reads",
            )
        })?;
        let mut ipc = ipc.lock().await;
        let document = ipc.document.clone();
        let header = || {
            Some(ItemHeader {
                document: Some(document.clone()),
                ..Default::default()
            })
        };
        let places = plan
            .operations
            .iter()
            .any(|o| matches!(o, BoardOp::PlaceFootprint { .. }));
        if places && self.provider == Provider::Headless {
            let _ = ipc
                .client
                .call_any(&LoadAllLibraries {
                    r#type: vec![LibraryType::LtFootprint as i32],
                })
                .await;
        }
        let client = &mut ipc.client;
        let commit: BeginCommitResponse = client
            .call(&BeginCommit { header: header() })
            .await
            .map_err(|e| from_ipc_error(&e, false))?;
        let message = match plan.kind {
            "outline" => "AI: board outline",
            "place" => "AI: place footprints",
            _ => "AI: move footprints",
        };
        let result = apply_steps(client, &header, plan).await;
        let action = if result.is_ok() {
            CommitAction::CmaCommit
        } else {
            CommitAction::CmaDrop
        };
        let ended = client
            .call_any(&EndCommit {
                id: commit.id,
                action: action as i32,
                message: message.into(),
                header: header(),
            })
            .await;
        match (result, ended) {
            (Ok(report), Ok(_)) => Ok(report),
            (Ok(_), Err(e)) => Err(from_ipc_error(&e, true)),
            (Err(err), Ok(_)) => Err(ToolError::new(
                err.code,
                format!("{} (the change was dropped; nothing changed)", err.message),
            )
            .with_mutation_state(MutationState::NotApplied)),
            (Err(err), Err(_)) => Err(ToolError::new(
                err.code,
                format!("{} (dropping the change failed too; read the board back)", err.message),
            )
            .with_mutation_state(MutationState::Unknown)),
        }
    }
}

async fn apply_steps(
    client: &mut Client,
    header: &impl Fn() -> Option<ItemHeader>,
    plan: &BoardEditPlan,
) -> Result<EditReport, ToolError> {
    let mut report = EditReport::default();

    // 1. The old outline.
    let remove: Vec<ProtoKiid> = plan
        .operations
        .iter()
        .filter_map(|o| match o {
            BoardOp::RemoveEdge { id, .. } => Some(ProtoKiid { value: id.clone() }),
            _ => None,
        })
        .collect();
    if !remove.is_empty() {
        let n = remove.len();
        let r: kicad_ipc::proto::kiapi::common::commands::DeleteItemsResponse = client
            .call(&DeleteItems {
                header: header(),
                item_ids: remove,
            })
            .await
            .map_err(|e| from_ipc_error(&e, true))?;
        let failed: Vec<String> = r
            .deleted_items
            .iter()
            .filter(|d| d.status != ItemDeletionStatus::IdsOk as i32)
            .map(|d| {
                format!(
                    "{} ({:?})",
                    d.id.as_ref().map(|k| k.value.as_str()).unwrap_or("?"),
                    d.status()
                )
            })
            .collect();
        if !failed.is_empty() {
            return Err(kerr(format!(
                "KiCad did not delete the old outline: {}",
                failed.join("; ")
            )));
        }
        report.removed_edges = n;
    }

    // 2. The new outline.
    for op in &plan.operations {
        let BoardOp::AddOutline {
            rect,
            corner_radius,
            line_width,
        } = op
        else {
            continue;
        };
        let shape = BoardGraphicShape {
            shape: Some(GraphicShape {
                attributes: Some(GraphicAttributes {
                    stroke: Some(StrokeAttributes {
                        width: Some(Distance { value_nm: line_width.0 }),
                        ..Default::default()
                    }),
                    fill: None,
                }),
                geometry: Some(graphic_shape::Geometry::Rectangle(GraphicRectangleAttributes {
                    top_left: vector(rect.min),
                    bottom_right: vector(rect.max),
                    corner_radius: Some(Distance {
                        value_nm: corner_radius.0,
                    }),
                })),
                ..Default::default()
            }),
            layer: BoardLayer::BlEdgeCuts as i32,
            ..Default::default()
        };
        let r: CreateItemsResponse = client
            .call(&CreateItems {
                header: header(),
                items: vec![Any::from_msg(&shape).map_err(|e| kerr(e.to_string()))?],
                ..Default::default()
            })
            .await
            .map_err(|e| from_ipc_error(&e, true))?;
        let created = r.created_items.first().map(|c| status_ok(&c.status));
        match created {
            Some(Ok(())) => report.outline_added = true,
            Some(Err(m)) => return Err(kerr(format!("KiCad did not create the outline: {m}"))),
            None => return Err(kerr("KiCad created no outline")),
        }
    }

    // 3. Footprints from the library (mounting holes). A footprint placed with
    // PlaceFootprintFromLibrary cannot be renamed until the commit ends (KiCad does not know it yet),
    // so the footprint is built here from the library's definition, named, moved to its place, and
    // created in the commit like any other item.
    for op in &plan.operations {
        let BoardOp::PlaceFootprint {
            reference,
            lib_id,
            at,
            reference_below,
        } = op
        else {
            continue;
        };
        let (library, name) = lib_id
            .split_once(':')
            .ok_or_else(|| ToolError::new(ErrorCode::InvalidArgument, format!("{lib_id} is not Library:Name")))?;
        let request = GetItemsFromLibrary {
            r#type: LibraryType::LtFootprint as i32,
            document: header().and_then(|h| h.document),
            item_ids: vec![LibraryIdentifier {
                library_nickname: library.into(),
                entry_name: name.into(),
            }],
        };
        // A headless server answers "not found" while it is still loading the libraries.
        let started = Instant::now();
        let definition: Footprint = loop {
            let answer: Result<GetItemsResponse, _> = client.call(&request).await;
            let found = match &answer {
                Ok(r) => r.items.first().and_then(|a| a.to_msg::<Footprint>().ok()),
                Err(_) => None,
            };
            match (found, answer) {
                (Some(d), _) => break d,
                (None, Err(e)) if !e.to_string().contains("not found") => return Err(from_ipc_error(&e, false)),
                _ if started.elapsed() < LIBRARY_LOAD_GRACE => tokio::time::sleep(Duration::from_secs(2)).await,
                _ => return Err(kerr(format!("KiCad does not find {lib_id} in its libraries"))),
            }
        };
        let mut instance = new_instance(definition, reference, name, *reference_below);
        Rigid {
            from: Point::default(),
            to: *at,
            deg: 0.0,
        }
        .footprint(&mut instance, reference)?;
        let r: CreateItemsResponse = client
            .call(&CreateItems {
                header: header(),
                items: vec![Any::from_msg(&instance).map_err(|e| kerr(e.to_string()))?],
                ..Default::default()
            })
            .await
            .map_err(|e| from_ipc_error(&e, true))?;
        match r.created_items.first().map(|c| status_ok(&c.status)) {
            Some(Ok(())) => report.placed.push(reference.clone()),
            Some(Err(m)) => return Err(kerr(format!("KiCad did not create {reference}: {m}"))),
            None => return Err(kerr(format!("KiCad created no {reference}"))),
        }
    }

    // 4. Moves: flips first (KiCad's own flip mirrors the footprint top to bottom), then every
    // footprint sent once with all its items turned and shifted, and its lock.
    let moves: Vec<(&String, &String, &kicad_domain::board_edit::Placement, bool)> = plan
        .operations
        .iter()
        .filter_map(|o| match o {
            BoardOp::MoveFootprint {
                reference,
                id,
                to,
                locked,
                ..
            } => Some((reference, id, to, *locked)),
            _ => None,
        })
        .collect();
    if moves.is_empty() {
        return Ok(report);
    }
    let current = footprints(client, header).await?;
    let find = |id: &str| current.iter().find(|f| f.id.as_ref().is_some_and(|k| k.value == id));
    let mut flip_ids = Vec::new();
    for (reference, id, to, _) in &moves {
        let f = find(id).ok_or_else(|| {
            ToolError::new(
                ErrorCode::StalePlan,
                format!("{reference} is no longer on the board; make a new plan"),
            )
        })?;
        let back = BoardLayer::try_from(f.layer).is_ok_and(|l| l == BoardLayer::BlBCu);
        if back != to.back {
            flip_ids.push(ProtoKiid { value: (*id).clone() });
            report.flipped.push((*reference).clone());
        }
    }
    if !flip_ids.is_empty() {
        let r: FlipItemsResponse = client
            .call(&FlipItems {
                header: header(),
                items: flip_ids,
                direction: BoardFlipDirection::BfdTopBottom as i32,
            })
            .await
            .map_err(|e| from_ipc_error(&e, true))?;
        let failed: Vec<String> = r
            .flipped_items
            .iter()
            .filter_map(|f| status_ok(&f.status).err())
            .collect();
        if !failed.is_empty() {
            return Err(kerr(format!(
                "KiCad did not flip every footprint: {}",
                failed.join("; ")
            )));
        }
    }
    let current = if report.flipped.is_empty() {
        current
    } else {
        footprints(client, header).await?
    };
    let mut items = Vec::new();
    for (reference, id, to, locked) in &moves {
        let mut f = current
            .iter()
            .find(|f| f.id.as_ref().is_some_and(|k| &k.value == *id))
            .cloned()
            .ok_or_else(|| kerr(format!("{reference} disappeared during the move")))?;
        let from = f.position.as_ref().map(point).unwrap_or_default();
        let deg = to.rotation_deg - f.orientation.as_ref().map(|a| a.value_degrees).unwrap_or(0.0);
        let rigid = Rigid {
            from,
            to: to.at,
            deg: if deg.abs() < 1e-9 { 0.0 } else { deg },
        };
        rigid.footprint(&mut f, reference)?;
        f.orientation = Some(Angle {
            value_degrees: to.rotation_deg,
        });
        f.locked = if *locked {
            LockedState::LsLocked
        } else {
            LockedState::LsUnlocked
        } as i32;
        items.push(f);
        report.moved.push((*reference).clone());
    }
    update(client, header, items).await?;
    Ok(report)
}
