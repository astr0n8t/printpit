//! Cheap checks that an uploaded model is what its extension says, before it's queued.
//!
//! printpit never parses meshes (that's the worker's job, in its sandbox). These checks only
//! catch the obvious: a renamed G-code, a photo, an HTML error page saved as `.stl`.

/// How much of the file's start [`check`] looks at.
pub const HEAD: usize = 4096;

/// `head` is the file's first bytes (up to [`HEAD`]), `len` its full size.
pub fn check(ext: &str, head: &[u8], len: u64) -> Result<(), String> {
    match ext {
        "stl" => stl(head, len),
        "3mf" => {
            if head.starts_with(b"PK\x03\x04") {
                Ok(())
            } else {
                Err("This .3mf file isn't a 3MF (zip) archive.".into())
            }
        }
        "step" | "stp" => {
            if text_start(head).starts_with(b"ISO-10303-21") {
                Ok(())
            } else {
                Err("This doesn't look like a STEP file (no ISO-10303-21 header).".into())
            }
        }
        "obj" => obj(head),
        _ => Err("Upload an .stl, .3mf, .obj or .step file.".into()),
    }
}

fn stl(head: &[u8], len: u64) -> Result<(), String> {
    // Binary: an 80-byte header, a triangle count, and exactly 50 bytes per triangle.
    if len >= 84 && head.len() >= 84 {
        let count = u32::from_le_bytes([head[80], head[81], head[82], head[83]]) as u64;
        if count > 0 && 84 + 50 * count == len {
            return Ok(());
        }
    }
    // ASCII: `solid`, then text. (Some binary files start with "solid" too; those passed above.)
    let start = text_start(head);
    if start.starts_with(b"solid") && !head.contains(&0) {
        let text = String::from_utf8_lossy(head);
        if text.contains("facet") || text.contains("endsolid") || len <= HEAD as u64 {
            return Ok(());
        }
    }
    Err("This doesn't look like an STL file.".into())
}

fn obj(head: &[u8]) -> Result<(), String> {
    if head.contains(&0) {
        return Err("This doesn't look like an OBJ file (it's binary).".into());
    }
    let text = String::from_utf8_lossy(head);
    let vertex = text
        .lines()
        .map(str::trim_start)
        .any(|l| l.starts_with("v ") || l.starts_with("v\t"));
    if vertex {
        Ok(())
    } else {
        Err("This doesn't look like an OBJ file (no vertices near the start).".into())
    }
}

/// Skips a UTF-8 BOM and leading whitespace.
fn text_start(head: &[u8]) -> &[u8] {
    let head = head.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(head);
    let skip = head.iter().take_while(|b| b.is_ascii_whitespace()).count();
    &head[skip..]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binary_stl(triangles: u32) -> Vec<u8> {
        let mut v = b"solid but actually binary".to_vec();
        v.resize(80, b' ');
        v.extend_from_slice(&triangles.to_le_bytes());
        v.resize(84 + 50 * triangles as usize, 0);
        v
    }

    fn head(b: &[u8]) -> &[u8] {
        &b[..b.len().min(HEAD)]
    }

    #[test]
    fn stls() {
        let b = binary_stl(12);
        assert_eq!(check("stl", head(&b), b.len() as u64), Ok(()));
        // Wrong length for its triangle count: truncated or not an STL.
        assert!(check("stl", head(&b), b.len() as u64 - 1).is_err());
        let ascii = b"solid cube\n facet normal 0 0 1\n  outer loop\n";
        assert_eq!(check("stl", ascii, 10_000), Ok(()));
        assert_eq!(check("stl", b"\xEF\xBB\xBF  solid x\nendsolid x\n", 22), Ok(()));
        assert!(check("stl", b"G28\nG1 X10\n", 11).is_err());
        assert!(check("stl", b"\x89PNG\r\n\x1a\n....", 2000).is_err());
        assert!(check("stl", b"<html><body>404</body></html>", 30).is_err());
    }

    #[test]
    fn others() {
        assert_eq!(check("3mf", b"PK\x03\x04rest", 100), Ok(()));
        assert!(check("3mf", b"solid", 100).is_err());
        assert_eq!(check("step", b"ISO-10303-21;\nHEADER;", 100), Ok(()));
        assert_eq!(check("stp", b"\n\nISO-10303-21;", 100), Ok(()));
        assert!(check("step", b"solid", 100).is_err());
        assert_eq!(
            check("obj", b"# made by hand\nmtllib x.mtl\nv 0 0 0\nv 1 0 0\n", 100),
            Ok(())
        );
        assert!(check("obj", b"# nothing here\n", 100).is_err());
        assert!(check("obj", b"v 0 0 0\0binary", 100).is_err());
        assert!(check("gcode", b"G28", 3).is_err());
    }
}
