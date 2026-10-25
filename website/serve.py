"""Local preview of the website, laid out the way GitHub Pages serves it.

    python website/serve.py [port]

Serves website/ at / and docs/media/ at /media/, the same layout the Pages workflow
(.github/workflows/pages.yml) builds, with HTTP range requests so the demo video can seek.
"""

import http.server
import os
import re
import sys
from pathlib import Path

SITE = Path(__file__).resolve().parent
MEDIA = SITE.parent / "docs" / "media"


class Handler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=str(SITE), **kwargs)

    def translate_path(self, path):
        clean = path.split("?", 1)[0].split("#", 1)[0]
        if clean.startswith("/media/"):
            return str(MEDIA / clean[len("/media/") :])
        return super().translate_path(path)

    def end_headers(self):
        self.send_header("Cache-Control", "no-store")
        super().end_headers()

    def send_head(self):
        match = re.fullmatch(r"bytes=(\d*)-(\d*)", self.headers.get("Range", ""))
        path = self.translate_path(self.path)
        if not match or not os.path.isfile(path):
            return super().send_head()
        size = os.path.getsize(path)
        start = int(match[1]) if match[1] else max(0, size - int(match[2] or 0))
        end = min(int(match[2]), size - 1) if match[1] and match[2] else size - 1
        if start > end:
            self.send_error(416)
            return None
        f = open(path, "rb")
        f.seek(start)
        self.send_response(206)
        self.send_header("Content-Type", self.guess_type(path))
        self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
        self.send_header("Content-Length", str(end - start + 1))
        self.send_header("Accept-Ranges", "bytes")
        self.end_headers()
        self._remaining = end - start + 1
        return f

    def copyfile(self, source, outputfile):
        remaining = getattr(self, "_remaining", None)
        if remaining is None:
            return super().copyfile(source, outputfile)
        while remaining > 0:
            chunk = source.read(min(64 * 1024, remaining))
            if not chunk:
                break
            outputfile.write(chunk)
            remaining -= len(chunk)


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8770
    Handler.extensions_map[".mp4"] = "video/mp4"
    with http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler) as httpd:
        print(f"Sidestage website on http://127.0.0.1:{port}/")
        httpd.serve_forever()
