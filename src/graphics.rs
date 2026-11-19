//! Kitty graphics protocol: image storage and cell-anchored placements.
//!
//! Pure data with no GTK dependency, so `PaneEngine` can own it and the UI
//! layer decodes bytes into textures. This is a vertical slice: transmit
//! (chunked), place, delete, and query, in PNG or raw RGB/RGBA. Animation
//! frames, file/shared-memory transmission, and unicode placeholders are out
//! of scope.

use base64::Engine as _;

pub const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_IMAGES: usize = 64;
pub const MAX_PLACEMENTS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    /// f=100, an encoded image. GDK decodes PNG and JPEG alike.
    Png,
    /// f=32, raw RGBA, width and height required.
    Rgba,
    /// f=24, raw RGB, width and height required.
    Rgb,
}

impl ImageFormat {
    fn from_code(code: u32) -> Option<Self> {
        match code {
            100 => Some(Self::Png),
            32 => Some(Self::Rgba),
            24 => Some(Self::Rgb),
            _ => None,
        }
    }

    pub fn bytes_per_pixel(self) -> Option<usize> {
        match self {
            Self::Png => None,
            Self::Rgba => Some(4),
            Self::Rgb => Some(3),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Image {
    pub id: u32,
    pub format: ImageFormat,
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// An image anchored to a grid line so it scrolls with the content it belongs
/// to. `line` uses the same absolute coordinate as OSC 133 prompt markers
/// (history_size + grid line), which is stable while history grows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub image_id: u32,
    pub placement_id: u32,
    pub line: i32,
    pub column: usize,
    pub cols: u16,
    pub rows: u16,
    pub z_index: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Query,
    Transmit,
    TransmitAndPlace,
    Place,
    Delete,
}

#[derive(Debug, Clone)]
pub struct Command {
    pub action: Action,
    pub format: ImageFormat,
    pub image_id: Option<u32>,
    pub placement_id: Option<u32>,
    pub width: u32,
    pub height: u32,
    pub cols: u16,
    pub rows: u16,
    pub z_index: i32,
    pub more: bool,
    /// q=1 suppresses the OK reply, q=2 suppresses errors too.
    pub quiet: u8,
    pub delete_target: Option<char>,
    pub payload: Vec<u8>,
}

impl Default for Command {
    fn default() -> Self {
        Self {
            action: Action::Transmit,
            format: ImageFormat::Png,
            image_id: None,
            placement_id: None,
            width: 0,
            height: 0,
            cols: 0,
            rows: 0,
            z_index: 0,
            more: false,
            quiet: 0,
            delete_target: None,
            payload: Vec::new(),
        }
    }
}

/// Parses the `G...` APC payload (without the leading G).
pub fn parse(payload: &[u8]) -> Result<Command, String> {
    let text = std::str::from_utf8(payload).map_err(|_| "graphics payload is not UTF-8")?;
    let (params, data) = match text.split_once(';') {
        Some((p, d)) => (p, d),
        None => (text, ""),
    };

    let mut cmd = Command::default();
    for pair in params.split(',') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        let num = |v: &str| {
            v.parse::<u32>()
                .map_err(|_| format!("bad numeric value for {key}"))
        };
        match key {
            "a" => {
                cmd.action = match value {
                    "q" => Action::Query,
                    "T" => Action::Transmit,
                    "t" => Action::TransmitAndPlace,
                    "p" => Action::Place,
                    "d" => Action::Delete,
                    other => return Err(format!("unsupported graphics action {other:?}")),
                }
            }
            "f" => {
                cmd.format = ImageFormat::from_code(num(value)?)
                    .ok_or_else(|| format!("unsupported graphics format {value}"))?
            }
            "i" => cmd.image_id = Some(num(value)?),
            "I" => cmd.image_id = Some(num(value)?),
            "p" => cmd.placement_id = Some(num(value)?),
            "s" => cmd.width = num(value)?,
            "v" => cmd.height = num(value)?,
            "c" => cmd.cols = u16::try_from(num(value)?).map_err(|_| "cols out of range")?,
            "r" => cmd.rows = u16::try_from(num(value)?).map_err(|_| "rows out of range")?,
            "z" => cmd.z_index = value.parse().map_err(|_| "bad z-index")?,
            "m" => cmd.more = value == "1",
            "q" => cmd.quiet = u8::try_from(num(value)?).unwrap_or(0),
            "d" => cmd.delete_target = value.chars().next(),
            // Keys we accept but do not act on in this slice.
            "C" | "x" | "y" | "X" | "Y" | "U" | "u" | "h" | "o" | "t" | "O" => {}
            _ => {}
        }
    }

    if !data.is_empty() {
        cmd.payload = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|e| format!("bad base64 payload: {e}"))?;
    }
    Ok(cmd)
}

#[derive(Debug, Default)]
struct Pending {
    cmd: Command,
    data: Vec<u8>,
}

#[derive(Debug, Default)]
pub struct GraphicsStore {
    images: Vec<Image>,
    placements: Vec<Placement>,
    pending: Option<Pending>,
    next_image_id: u32,
    next_placement_id: u32,
    /// Bumped on every mutation so the renderer can invalidate its texture
    /// cache without comparing image contents.
    generation: u64,
}

impl GraphicsStore {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn images(&self) -> &[Image] {
        &self.images
    }

    pub fn placements(&self) -> &[Placement] {
        &self.placements
    }

    pub fn image(&self, id: u32) -> Option<&Image> {
        self.images.iter().find(|i| i.id == id)
    }

    pub fn is_empty(&self) -> bool {
        self.images.is_empty() && self.placements.is_empty()
    }

    fn bump(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    fn alloc_image_id(&mut self, requested: Option<u32>) -> Result<u32, String> {
        if let Some(id) = requested {
            if id == 0 {
                return Err(String::from("image id 0 is reserved"));
            }
            if self.images.iter().any(|i| i.id == id) {
                return Err(format!("image id {id} already exists"));
            }
            return Ok(id);
        }
        self.next_image_id = self.next_image_id.saturating_add(1).max(1);
        Ok(self.next_image_id)
    }

    fn alloc_placement_id(&mut self, requested: Option<u32>) -> Result<u32, String> {
        if let Some(id) = requested {
            if self.placements.iter().any(|p| p.placement_id == id) {
                return Err(format!("placement id {id} already exists"));
            }
            return Ok(id);
        }
        self.next_placement_id = self.next_placement_id.saturating_add(1).max(1);
        Ok(self.next_placement_id)
    }

    /// Applies a command. `line` and `column` are the cursor anchor for a new
    /// placement, in the absolute coordinate described on `Placement`.
    /// Returns the image id when one was created or referenced.
    pub fn apply(
        &mut self,
        mut cmd: Command,
        line: i32,
        column: usize,
    ) -> Result<Option<u32>, String> {
        // Chunked transmission: accumulate until m=0.
        if cmd.more {
            let entry = self.pending.get_or_insert_with(|| Pending {
                cmd: cmd.clone(),
                data: Vec::new(),
            });
            if entry.data.len() + cmd.payload.len() > MAX_IMAGE_BYTES {
                self.pending = None;
                return Err(String::from("image exceeds the size limit"));
            }
            entry.data.extend_from_slice(&cmd.payload);
            return Ok(None);
        }
        if let Some(mut pending) = self.pending.take() {
            if pending.data.len() + cmd.payload.len() > MAX_IMAGE_BYTES {
                return Err(String::from("image exceeds the size limit"));
            }
            pending.data.extend_from_slice(&cmd.payload);
            // The first chunk carries the parameters; later chunks may omit
            // them, so keep the original action and format.
            cmd.action = pending.cmd.action;
            cmd.format = pending.cmd.format;
            cmd.width = pending.cmd.width;
            cmd.height = pending.cmd.height;
            if cmd.image_id.is_none() {
                cmd.image_id = pending.cmd.image_id;
            }
            if cmd.cols == 0 {
                cmd.cols = pending.cmd.cols;
            }
            if cmd.rows == 0 {
                cmd.rows = pending.cmd.rows;
            }
            cmd.payload = pending.data;
        }

        match cmd.action {
            Action::Query => Ok(None),
            Action::Delete => {
                self.delete(&cmd);
                Ok(None)
            }
            Action::Place => {
                let id = cmd
                    .image_id
                    .ok_or_else(|| String::from("place requires an image id"))?;
                if self.image(id).is_none() {
                    return Err(format!("no image with id {id}"));
                }
                let pid = self.alloc_placement_id(cmd.placement_id)?;
                self.push_placement(Placement {
                    image_id: id,
                    placement_id: pid,
                    line,
                    column,
                    cols: cmd.cols,
                    rows: cmd.rows,
                    z_index: cmd.z_index,
                });
                Ok(Some(id))
            }
            Action::Transmit | Action::TransmitAndPlace => {
                if cmd.payload.len() > MAX_IMAGE_BYTES {
                    return Err(String::from("image exceeds the size limit"));
                }
                if let Some(bpp) = cmd.format.bytes_per_pixel() {
                    if cmd.width == 0 || cmd.height == 0 {
                        return Err(String::from("raw format requires s and v"));
                    }
                    let expected = cmd.width as usize * cmd.height as usize * bpp;
                    if cmd.payload.len() != expected {
                        return Err(format!(
                            "raw payload is {} bytes, expected {expected}",
                            cmd.payload.len()
                        ));
                    }
                }
                let id = self.alloc_image_id(cmd.image_id)?;
                if self.images.len() >= MAX_IMAGES {
                    // Evict the oldest image and any placements referencing it.
                    let evicted = self.images.remove(0).id;
                    self.placements.retain(|p| p.image_id != evicted);
                }
                self.images.push(Image {
                    id,
                    format: cmd.format,
                    width: cmd.width,
                    height: cmd.height,
                    data: cmd.payload,
                });
                self.bump();
                if cmd.action == Action::TransmitAndPlace {
                    let pid = self.alloc_placement_id(cmd.placement_id)?;
                    self.push_placement(Placement {
                        image_id: id,
                        placement_id: pid,
                        line,
                        column,
                        cols: cmd.cols,
                        rows: cmd.rows,
                        z_index: cmd.z_index,
                    });
                }
                Ok(Some(id))
            }
        }
    }

    fn push_placement(&mut self, placement: Placement) {
        if self.placements.len() >= MAX_PLACEMENTS {
            self.placements.remove(0);
        }
        self.placements.push(placement);
        self.bump();
    }

    fn delete(&mut self, cmd: &Command) {
        // No target, or d=a with no id: clear everything.
        match cmd.delete_target {
            Some('a') | None if cmd.image_id.is_none() && cmd.placement_id.is_none() => {
                self.images.clear();
                self.placements.clear();
                self.bump();
                return;
            }
            _ => {}
        }
        if let Some(pid) = cmd.placement_id {
            let before = self.placements.len();
            self.placements.retain(|p| p.placement_id != pid);
            if self.placements.len() != before {
                self.bump();
            }
        }
        if let Some(id) = cmd.image_id {
            let before = self.images.len();
            self.images.retain(|i| i.id != id);
            self.placements.retain(|p| p.image_id != id);
            if self.images.len() != before {
                self.bump();
            }
        }
    }

    /// Convenience for the present-media verb: store an encoded image and
    /// place it at the cursor in one step.
    pub fn present(
        &mut self,
        data: Vec<u8>,
        cols: u16,
        rows: u16,
        line: i32,
        column: usize,
    ) -> Result<(u32, u32), String> {
        if data.len() > MAX_IMAGE_BYTES {
            return Err(String::from("image exceeds the size limit"));
        }
        let id = self.alloc_image_id(None)?;
        if self.images.len() >= MAX_IMAGES {
            let evicted = self.images.remove(0).id;
            self.placements.retain(|p| p.image_id != evicted);
        }
        self.images.push(Image {
            id,
            format: ImageFormat::Png,
            width: 0,
            height: 0,
            data,
        });
        let pid = self.alloc_placement_id(None)?;
        self.push_placement(Placement {
            image_id: id,
            placement_id: pid,
            line,
            column,
            cols,
            rows,
            z_index: 0,
        });
        Ok((id, pid))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(data: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(data)
    }

    #[test]
    fn parses_transmit_and_place_params() {
        let payload = format!("Gf=100,i=7,c=20,r=10,z=3,a=t;{}", b64(b"pngbytes"));
        let cmd = parse(payload.as_bytes().strip_prefix(b"G").unwrap()).expect("parse");
        assert_eq!(cmd.action, Action::TransmitAndPlace);
        assert_eq!(cmd.format, ImageFormat::Png);
        assert_eq!(cmd.image_id, Some(7));
        assert_eq!((cmd.cols, cmd.rows), (20, 10));
        assert_eq!(cmd.z_index, 3);
        assert_eq!(cmd.payload, b"pngbytes");
    }

    #[test]
    fn transmit_then_place_creates_a_cell_anchored_placement() {
        let mut store = GraphicsStore::default();
        let cmd = parse(format!("Ga=T,f=100,i=1;{}", b64(b"img")).as_bytes()[1..].as_ref())
            .expect("parse");
        assert_eq!(store.apply(cmd, 100, 4).unwrap(), Some(1));
        assert!(store.placements().is_empty(), "a=T must not place");

        let place = parse(b"a=p,i=1,c=8,r=4").expect("parse");
        store.apply(place, 100, 4).expect("place");
        let p = store.placements()[0];
        assert_eq!(
            (p.image_id, p.line, p.column, p.cols, p.rows),
            (1, 100, 4, 8, 4)
        );
    }

    #[test]
    fn chunked_transmission_reassembles() {
        let mut store = GraphicsStore::default();
        let first = parse(format!("Ga=t,f=100,m=1;{}", b64(b"hello ")).as_bytes()[1..].as_ref())
            .expect("first");
        assert_eq!(store.apply(first, 0, 0).unwrap(), None);
        assert!(store.images().is_empty(), "must not store a partial image");

        let last = parse(format!("Gm=0;{}", b64(b"world")).as_bytes()[1..].as_ref()).expect("last");
        let id = store.apply(last, 5, 2).unwrap().expect("id");
        assert_eq!(store.image(id).unwrap().data, b"hello world");
        assert_eq!(
            store.placements().len(),
            1,
            "a=t from the first chunk places"
        );
    }

    #[test]
    fn raw_formats_validate_dimensions() {
        let mut store = GraphicsStore::default();
        let bad = parse(b"a=T,f=32,s=2,v=2;AAAA").expect("parse");
        assert!(
            store.apply(bad, 0, 0).is_err(),
            "wrong byte count must fail"
        );

        let good =
            parse(format!("Ga=T,f=32,s=1,v=1;{}", b64(&[1, 2, 3, 4])).as_bytes()[1..].as_ref())
                .expect("parse");
        assert!(store.apply(good, 0, 0).is_ok());

        let no_dims = parse(format!("Ga=T,f=24;{}", b64(&[1, 2, 3])).as_bytes()[1..].as_ref())
            .expect("parse");
        assert!(store.apply(no_dims, 0, 0).is_err(), "raw needs s and v");
    }

    #[test]
    fn delete_removes_images_and_their_placements() {
        let mut store = GraphicsStore::default();
        let cmd =
            parse(format!("Ga=t,f=100,i=9;{}", b64(b"x")).as_bytes()[1..].as_ref()).expect("parse");
        store.apply(cmd, 0, 0).unwrap();
        assert_eq!(store.placements().len(), 1);

        store.apply(parse(b"a=d,d=i,i=9").unwrap(), 0, 0).unwrap();
        assert!(store.images().is_empty());
        assert!(
            store.placements().is_empty(),
            "placements must go with the image"
        );

        store
            .apply(
                parse(format!("Ga=t,f=100;{}", b64(b"y")).as_bytes()[1..].as_ref()).unwrap(),
                0,
                0,
            )
            .unwrap();
        store.apply(parse(b"a=d,d=a").unwrap(), 0, 0).unwrap();
        assert!(store.is_empty());
    }

    #[test]
    fn generation_changes_on_mutation_only() {
        let mut store = GraphicsStore::default();
        let g0 = store.generation();
        store.apply(parse(b"a=q").unwrap(), 0, 0).unwrap();
        assert_eq!(store.generation(), g0, "query must not invalidate textures");
        store
            .apply(
                parse(format!("Ga=T,f=100;{}", b64(b"z")).as_bytes()[1..].as_ref()).unwrap(),
                0,
                0,
            )
            .unwrap();
        assert_ne!(store.generation(), g0);
    }

    #[test]
    fn present_stores_and_places_in_one_step() {
        let mut store = GraphicsStore::default();
        let (id, pid) = store
            .present(b"png".to_vec(), 30, 12, 42, 1)
            .expect("present");
        assert_eq!(store.image(id).unwrap().data, b"png");
        let p = store
            .placements()
            .iter()
            .find(|p| p.placement_id == pid)
            .unwrap();
        assert_eq!((p.line, p.column, p.cols, p.rows), (42, 1, 30, 12));
    }

    #[test]
    fn oversized_and_duplicate_ids_are_rejected() {
        let mut store = GraphicsStore::default();
        let cmd = parse(format!("Ga=T,f=100,i=1;{}", b64(b"a")).as_bytes()[1..].as_ref()).unwrap();
        store.apply(cmd, 0, 0).unwrap();
        let dup = parse(format!("Ga=T,f=100,i=1;{}", b64(b"b")).as_bytes()[1..].as_ref()).unwrap();
        assert!(store.apply(dup, 0, 0).is_err());

        let big = vec![0u8; MAX_IMAGE_BYTES + 1];
        assert!(store.present(big, 1, 1, 0, 0).is_err());
    }
}
