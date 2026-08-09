# SPDX-License-Identifier: AGPL-3.0-only
#
# dragnet.py nova3 plugin'i için offline (stub HTTP sunucu) testleri.
# Canlı dragnetd gerektirmez; yalnızca Python standart kütüphanesi.
#
# Çalıştırma:
#   py plugins/qbittorrent/test_dragnet.py
#   (veya: python -m unittest plugins/qbittorrent/test_dragnet.py)

import importlib.util
import json
import os
import sys
import threading
import types
import unittest
from http.server import BaseHTTPRequestHandler, HTTPServer

PLUGIN_PATH = os.path.join(os.path.dirname(__file__), "dragnet.py")

# --- Sahte API verisi ---
_FAKE_RESULTS = {
    "results": [
        {
            "infohash": "08ada5a7a6183aae1e09d831df6748d566095a10",
            "name": "Sintel",
            "size": 129302391,
            "seeds": -1,
            "leech": -1,
            "pub_date": 1700000000,
        }
    ]
}


class _StubHandler(BaseHTTPRequestHandler):
    def do_GET(self):
        # /search → sonuç; başka her şey boş.
        if self.path.startswith("/search"):
            body = json.dumps(_FAKE_RESULTS).encode("utf-8")
        else:
            body = json.dumps({"results": []}).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass  # sessiz


def _load_plugin(base_url):
    """dragnet.py'yi sahte novaprinter ile yükler; yakalanan çıktı listesini döner."""
    captured = []
    np = types.ModuleType("novaprinter")
    np.prettyPrinter = lambda d: captured.append(d)
    sys.modules["novaprinter"] = np

    spec = importlib.util.spec_from_file_location("dragnet_plugin_under_test", PLUGIN_PATH)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)

    engine = mod.dragnet()
    engine.base_url = base_url
    return engine, captured


class DragnetPluginTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = HTTPServer(("127.0.0.1", 0), _StubHandler)
        cls.port = cls.server.server_address[1]
        cls.thread = threading.Thread(target=cls.server.serve_forever, daemon=True)
        cls.thread.start()
        cls.base_url = f"http://127.0.0.1:{cls.port}"

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()

    def test_search_prints_prettyprinter_row(self):
        engine, captured = _load_plugin(self.base_url)
        engine.search("sintel")

        self.assertEqual(len(captured), 1)
        row = captured[0]
        # nova3 prettyPrinter sözleşmesi: gerekli alanlar mevcut.
        self.assertTrue(
            row["link"].startswith(
                "magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a10"
            )
        )
        self.assertIn("dn=Sintel", row["link"])
        self.assertEqual(row["name"], "Sintel")
        self.assertEqual(row["size"], 129302391)
        self.assertEqual(row["seeds"], -1)
        self.assertEqual(row["leech"], -1)
        self.assertTrue(row["engine_url"])

    def test_service_down_is_silent(self):
        # Kapalı bir porta işaret et → istisna yutulur, hiçbir şey basılmaz.
        engine, captured = _load_plugin("http://127.0.0.1:1")
        engine.search("sintel")
        self.assertEqual(captured, [])


if __name__ == "__main__":
    unittest.main(verbosity=2)
