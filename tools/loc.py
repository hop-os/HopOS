"""Regels Rust-code zonder commentaar en lege regels, apart: code en toetsen.

Toetsen: bestanden tests*.rs / test.rs / *_tests.rs / testutil.rs / onder
tests/ of benches/, en blokken na #[cfg(test)]. *_tests.rs en testutil.rs
staan in de boom alleen achter een #[cfg(test)] mod; tools/loc ziet dat
vanzelf (ze staan in geen dep-info van een gewone build). Strings (ook r#"..."#) en char-literals worden overgeslagen
zodat "https://" geen commentaar wordt.
"""
import os
import sys


def strip(src):
    """Geeft per regel True als er code (geen commentaar/witruimte) op staat,
    en de tekst zonder commentaar."""
    out = []
    i, n = 0, len(src)
    depth = 0  # /* */ nest
    line_has = False
    buf = []
    lines = []
    while i < n:
        c = src[i]
        if c == '\n':
            lines.append((line_has, ''.join(buf)))
            line_has = False
            buf = []
            i += 1
            continue
        if depth:
            if src.startswith('*/', i):
                depth -= 1
                i += 2
            elif src.startswith('/*', i):
                depth += 1
                i += 2
            else:
                i += 1
            continue
        if src.startswith('//', i):
            while i < n and src[i] != '\n':
                i += 1
            continue
        if src.startswith('/*', i):
            depth = 1
            i += 2
            continue
        if c == 'r' and i + 1 < n and src[i + 1] in '#"' and (i == 0 or not (src[i - 1].isalnum() or src[i - 1] == '_')):
            j = i + 1
            h = 0
            while j < n and src[j] == '#':
                h += 1
                j += 1
            if j < n and src[j] == '"':
                end = '"' + '#' * h
                k = src.find(end, j + 1)
                k = n if k < 0 else k + len(end)
                seg = src[i:k]
                for p, part in enumerate(seg.split('\n')):
                    if p:
                        lines.append((line_has, ''.join(buf)))
                        buf = []
                    line_has = True
                    buf.append('S')
                i = k
                continue
        if c == '"':
            j = i + 1
            while j < n and src[j] != '"':
                j += 2 if src[j] == '\\' else 1
            seg = src[i:j + 1]
            for p, part in enumerate(seg.split('\n')):
                if p:
                    lines.append((line_has, ''.join(buf)))
                    buf = []
                line_has = True
                buf.append('S')
            i = j + 1
            continue
        if c == "'":
            # char-literal of lifetime
            if i + 2 < n and src[i + 1] == '\\':
                k = src.find("'", i + 2)
                if k > 0 and k - i < 12:
                    line_has = True
                    buf.append('C')
                    i = k + 1
                    continue
            if i + 2 < n and src[i + 2] == "'":
                line_has = True
                buf.append('C')
                i += 3
                continue
        if not c.isspace():
            line_has = True
        buf.append(c)
        i += 1
    lines.append((line_has, ''.join(buf)))
    return lines


def count(path):
    src = open(path, encoding='utf-8', errors='replace').read()
    lines = strip(src)
    base = os.path.basename(path)
    parts = path.split(os.sep)
    whole_test = (base.startswith('tests') or base.endswith('_tests.rs') or base in ('test.rs', 'testutil.rs')
                  or 'tests' in parts or 'benches' in parts)
    code = test = 0
    in_test = 0  # brace depth of cfg(test) block, 0 = not
    pending = False
    for has, text in lines:
        if not has:
            continue
        t = text.strip()
        if whole_test:
            test += 1
            continue
        if in_test:
            test += 1
            in_test += text.count('{') - text.count('}')
            if in_test <= 0:
                in_test = 0
            continue
        if pending:
            test += 1
            d = text.count('{') - text.count('}')
            if '{' in text:
                pending = False
                if d > 0:
                    in_test = d
            elif t.endswith(';'):
                pending = False
            continue
        if t.startswith('#[cfg(test)]') or t.startswith('#[cfg(all(test'):
            test += 1
            rest = t.split(']', 1)[1] if ']' in t else ''
            if '{' in rest:
                d = rest.count('{') - rest.count('}')
                if d > 0:
                    in_test = d
            elif rest.strip().endswith(';'):
                pass
            else:
                pending = True
            continue
        code += 1
    return code, test


def walk(root, skip=()):
    c = t = 0
    for d, dirs, files in os.walk(root):
        dirs[:] = [x for x in dirs if x not in ('target', 'OLD', '.git', 'archive') and os.path.join(d, x) not in skip]
        for f in files:
            if f.endswith('.rs'):
                a, b = count(os.path.join(d, f))
                c += a
                t += b
    return c, t


if __name__ == '__main__':
    for arg in sys.argv[1:]:
        c, t = walk(arg)
        print(f'{c:8d} {t:8d}  {arg}')
