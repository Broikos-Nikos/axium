"""Persistent markdown memory, survives across sessions.

One file, `## Section` headings, appended or replaced per section. Deliberately
plain text so a human can read and edit it.
"""
import os
import re
import threading

_LOCK = threading.Lock()


class Memory:
    def __init__(self, path):
        self.path = os.path.abspath(path)
        os.makedirs(os.path.dirname(self.path) or ".", exist_ok=True)
        if not os.path.exists(self.path):
            self._write("# Memory\n")

    # -- io --
    def _read(self):
        with open(self.path, encoding="utf-8") as f:
            return f.read()

    def _write(self, text):
        tmp = self.path + ".tmp"
        with open(tmp, "w", encoding="utf-8") as f:
            f.write(text)
        os.replace(tmp, self.path)

    @property
    def content(self):
        return self._read()

    def sections(self):
        """{heading: body} for every `## ` section."""
        out, current, buf = {}, None, []
        for line in self._read().splitlines():
            m = re.match(r"^##\s+(.*)$", line)
            if m:
                if current is not None:
                    out[current] = "\n".join(buf).strip()
                current, buf = m.group(1).strip(), []
            elif current is not None:
                buf.append(line)
        if current is not None:
            out[current] = "\n".join(buf).strip()
        return out

    def append_to_section(self, section, content):
        with _LOCK:
            text = self._read()
            pattern = re.compile(rf"^##\s+{re.escape(section)}\s*$", re.M)
            m = pattern.search(text)
            if not m:
                self._write(f"{text.rstrip()}\n\n## {section}\n{content.strip()}\n")
                return
            nxt = re.compile(r"^##\s+", re.M).search(text, m.end())
            end = nxt.start() if nxt else len(text)
            body = text[m.end():end].rstrip()
            self._write(text[:m.end()] + f"\n{body}\n{content.strip()}\n\n" + text[end:])

    def replace_section(self, section, content):
        with _LOCK:
            text = self._read()
            pattern = re.compile(rf"^##\s+{re.escape(section)}\s*$", re.M)
            m = pattern.search(text)
            if not m:
                self._write(f"{text.rstrip()}\n\n## {section}\n{content.strip()}\n")
                return
            nxt = re.compile(r"^##\s+", re.M).search(text, m.end())
            end = nxt.start() if nxt else len(text)
            self._write(text[:m.end()] + f"\n{content.strip()}\n\n" + text[end:])
