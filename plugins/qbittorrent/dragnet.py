# VERSION: 0.2.0
# SPDX-License-Identifier: AGPL-3.0-only
#
# Dragnet — qBittorrent nova3 arama plugin'i
#
# Bu dosya qBittorrent'in arama-plugin (nova3 `engines/`) dizinine kopyalanır.
# qBittorrent kaynağına HİÇBİR değişiklik gerektirmez. Yerelde çalışan Dragnet
# HTTP API'sine (dragnetd) sorgu atar ve sonuçları nova3 sözleşmesine göre yazdırır.
#
# Kurulum ve sözleşme ayrıntısı: docs/INTEGRATION.md

import json
import os
import urllib.parse
import urllib.request

# nova3 çalışma zamanında bu modülleri sağlar (qBittorrent plugin dizininde bulunur).
from novaprinter import prettyPrinter  # type: ignore

# Dragnet API kök adresi. Öncelik: DRAGNET_API_URL ortam değişkeni, yoksa varsayılan.
# dragnetd'nin api_bind değeriyle eşleşmelidir.
_BASE_URL = os.environ.get("DRAGNET_API_URL", "http://127.0.0.1:8080")


class dragnet:
    """Dragnet DHT indeksine karşı arama yapan nova3 motoru."""

    # Yerelde çalışan Dragnet API'si. Gerekirse doğrudan burayı düzenleyin.
    base_url: str = _BASE_URL

    url: str = _BASE_URL
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
