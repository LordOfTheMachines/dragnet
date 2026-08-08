# VERSION: 0.1.0
# SPDX-License-Identifier: AGPL-3.0-only
#
# Dragnet — qBittorrent nova3 arama plugin'i (TASLAK)
#
# Bu dosya qBittorrent'in arama-plugin (nova3 `engines/`) dizinine kopyalanır.
# qBittorrent kaynağına HİÇBİR değişiklik gerektirmez. Yerelde çalışan Dragnet
# HTTP API'sine sorgu atar ve sonuçları nova3 sözleşmesine göre yazdırır.
#
# Sözleşme ayrıntısı: docs/INTEGRATION.md

import json
import urllib.parse
import urllib.request

# nova3 çalışma zamanında bu modülleri sağlar (qBittorrent plugin dizininde bulunur).
from novaprinter import prettyPrinter  # type: ignore


class dragnet:
    """Dragnet DHT indeksine karşı arama yapan nova3 motoru."""

    # Yerelde çalışan Dragnet API'si. Gerekirse değiştirin.
    base_url: str = "http://127.0.0.1:8080"

    url: str = "http://127.0.0.1:8080"
    name: str = "Dragnet"
    supported_categories: dict[str, str] = {
        "all": "all",
        "anime": "anime",
        "books": "books",
        "games": "games",
        "movies": "movies",
        "music": "music",
        "software": "software",
        "tv": "tv",
    }

    def search(self, what: str, cat: str = "all") -> None:
        query = urllib.parse.urlencode({"q": what, "cat": cat, "limit": 100})
        request_url = f"{self.base_url}/search?{query}"

        try:
            with urllib.request.urlopen(request_url, timeout=15) as response:
                payload = json.loads(response.read().decode("utf-8"))
        except Exception:
            # Dragnet servisi kapalıysa sessizce sonuç yok; nova3 bunu tolere eder.
            return

        for item in payload.get("results", []):
            infohash = item.get("infohash", "")
            name = item.get("name", "")
            if not infohash or not name:
                continue

            magnet = "magnet:?xt=urn:btih:%s&dn=%s" % (
                infohash,
                urllib.parse.quote(name),
            )

            prettyPrinter(
                {
                    "link": magnet,
                    "name": name,
                    "size": item.get("size", -1),
                    "seeds": item.get("seeds", -1),
                    "leech": item.get("leech", -1),
                    "engine_url": self.url,
                    "pub_date": item.get("pub_date", -1),
                }
            )


# nova2.py plugin'i doğrudan çalıştırırsa kaba bir kendi kendine test yapılabilir:
if __name__ == "__main__":
    import sys

    engine = dragnet()
    engine.search(sys.argv[1] if len(sys.argv) > 1 else "ubuntu")
