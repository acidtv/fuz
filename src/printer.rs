#[inline(always)]
pub fn emit_match(out: &mut Vec<u8>, prefix: &[u8], line_no: u64, col: u32, line: &[u8]) {
    // vimgrep format: path:line:col:content
    // Worst-case: prefix + 20-digit line_no + ':' + 10-digit col + ':' + line + '\n'
    out.reserve(prefix.len() + 33 + line.len());
    out.extend_from_slice(prefix);
    write_u64(out, line_no);
    out.push(b':');
    write_u64(out, col as u64);
    out.push(b':');
    out.extend_from_slice(line);
    out.push(b'\n');
}

#[inline]
fn write_u64(out: &mut Vec<u8>, mut n: u64) {
    if n == 0 {
        out.push(b'0');
        return;
    }
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    while n > 0 {
        i -= 1;
        tmp[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    out.extend_from_slice(&tmp[i..]);
}
