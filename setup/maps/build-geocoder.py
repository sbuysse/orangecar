#!/usr/bin/env python3
"""Offline destination search index for the dashboard's navigation map.

Reads OpenStreetMap extracts and writes one SQLite file with an FTS5 table that
rascar-app queries (/api/geo/search):

  place    towns, villages, suburbs (place=*)
  street   one row per street name per town (centre of its first way)
  address  house numbers (addr:housenumber + addr:street)
  poi      named amenities/shops/tourism/leisure (fuel, parking, restaurant...)

Each row: name (what the user types), detail (town, postcode, kind), lat, lon,
kind, rank (towns first). Matching is accent-insensitive (unicode61
remove_diacritics). A street or address without addr:city takes the nearest
place node's name — an approximation, good enough to tell two "Rue de la
Station" apart.

    pip install osmium
    setup/maps/build-geocoder.py out.sqlite osm/*.osm.pbf
"""
import math
import sqlite3
import sys
import time

import osmium

PLACE_RANK = {"city": 100, "town": 80, "village": 60, "suburb": 50, "quarter": 45, "hamlet": 40, "neighbourhood": 35, "locality": 30}
POI_KEYS = ("amenity", "shop", "tourism", "leisure")
CELL = 0.1  # degrees, nearest-place grid


def place_nodes(files):
    """First pass: place nodes, to name the town of streets and addresses.
    The key filter runs in C++: Python only sees the few thousand place nodes."""
    out = []
    for f in files:
        for n in osmium.FileProcessor(f, osmium.osm.NODE).with_filter(osmium.filter.KeyFilter("place")):
            kind, name = n.tags.get("place"), n.tags.get("name")
            if kind in PLACE_RANK and name:
                out.append((name, kind, n.location.lat, n.location.lon, n.tags.get("addr:postcode", "")))
    return out


class Grid:
    """Nearest place node lookups. Two layers: the municipality (city, town,
    village) — what people type — and the neighbourhood (suburb, quarter,
    hamlet…), which alone hid the town: a shop filed under its district was
    not found by "<shop> <town>"."""

    def __init__(self, places, kinds):
        self.cells = {}
        for p in places:
            if p[1] in kinds:
                self.cells.setdefault((int(p[2] // CELL), int(p[3] // CELL)), []).append(p)

    def nearest(self, lat, lon):
        cy, cx = int(lat // CELL), int(lon // CELL)
        best, bd = None, 1e18
        for r in (1, 2):
            for y in range(cy - r, cy + r + 1):
                for x in range(cx - r, cx + r + 1):
                    for p in self.cells.get((y, x), ()):
                        d = (p[2] - lat) ** 2 + ((p[3] - lon) * math.cos(math.radians(lat))) ** 2
                        if d < bd:
                            best, bd = p, d
            if best:
                return best[0], bd
        return "", 1e18


TOWN_KINDS = {"city", "town", "village"}
LOCAL_KINDS = {"suburb", "quarter", "hamlet", "neighbourhood"}


class Index:
    def __init__(self, db, places):
        self.db = db
        self.towns = Grid(places, TOWN_KINDS)
        self.locals = Grid(places, LOCAL_KINDS)
        self.rows = []
        self.streets = set()
        self.n = 0

    def add(self, name, detail, kind, lat, lon, rank):
        self.rows.append((name, detail, kind, lat, lon, rank))
        if len(self.rows) >= 50000:
            self.flush()

    def flush(self):
        self.db.executemany("INSERT INTO place(name, detail, kind, lat, lon, rank) VALUES (?,?,?,?,?,?)", self.rows)
        self.n += len(self.rows)
        self.rows = []

    def town(self, tags, lat, lon):
        """ "Neighbourhood, Town" (or just the town): both are searchable."""
        town, td = self.towns.nearest(lat, lon)
        town = tags.get("addr:city") or town
        local, ld = self.locals.nearest(lat, lon)
        if local and local != town and ld < td:
            return f"{local}, {town}" if town else local
        return town

    def element(self, tags, lat, lon):
        name = tags.get("name")
        kind = tags.get("place")
        if kind in PLACE_RANK and name:
            pc = tags.get("addr:postcode", "")
            self.add(name, f"{pc} {kind}".strip(), "place", lat, lon, PLACE_RANK[kind])
            return
        hn, street = tags.get("addr:housenumber"), tags.get("addr:street")
        if hn and street:
            town = self.town(tags, lat, lon)
            pc = tags.get("addr:postcode", "")
            self.add(f"{street} {hn}", f"{pc} {town}".strip(), "address", lat, lon, 10)
        if name:
            for k in POI_KEYS:
                v = tags.get(k)
                if v:
                    town = self.town(tags, lat, lon)
                    self.add(name, f"{v.replace('_', ' ')} · {town}".strip(" ·"), "poi", lat, lon, 20)
                    break

    def way(self, w):
        tags = w.tags
        try:
            loc = w.nodes[len(w.nodes) // 2].location
            lat, lon = loc.lat, loc.lon
        except (osmium.InvalidLocationError, IndexError):
            return
        hw, name = tags.get("highway"), tags.get("name")
        # Named squares count as streets (a town square is place=square).
        if name and ((hw and hw not in ("bus_stop", "platform", "proposed", "construction")) or tags.get("place") == "square"):
            town = self.town(tags, lat, lon)
            key = (name, town)
            if key not in self.streets:
                self.streets.add(key)
                self.add(name, town, "street", lat, lon, 25)
            return
        self.element(tags, lat, lon)

    def apply(self, f):
        # Only tagged objects carrying a key we index reach Python; node
        # locations (for way centres) are kept by osmium in C++.
        keys = ("place", "addr:housenumber", "highway", *POI_KEYS)
        fp = (osmium.FileProcessor(f, osmium.osm.NODE | osmium.osm.WAY)
              .with_locations("flex_mem")
              .with_filter(osmium.filter.KeyFilter(*keys)))
        for o in fp:
            if o.is_node():
                self.element(o.tags, o.location.lat, o.location.lon)
            elif o.is_way():
                self.way(o)


def main():
    out, files = sys.argv[1], sys.argv[2:]
    t0 = time.time()
    places = place_nodes(files)
    print(f"geocoder: {len(places)} place nodes ({time.time() - t0:.0f} s)", flush=True)
    db = sqlite3.connect(out)
    db.executescript("""
        DROP TABLE IF EXISTS place; DROP TABLE IF EXISTS place_fts;
        CREATE TABLE place(id INTEGER PRIMARY KEY, name TEXT, detail TEXT, kind TEXT, lat REAL, lon REAL, rank INTEGER);
        PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF;
    """)
    idx = Index(db, places)
    for f in files:
        idx.apply(f)
        idx.flush()
        db.commit()
        print(f"geocoder: {f}: {idx.n} rows ({time.time() - t0:.0f} s)", flush=True)
    db.executescript("""
        CREATE VIRTUAL TABLE place_fts USING fts5(name, detail, content='place', content_rowid='id',
            tokenize='unicode61 remove_diacritics 2', prefix='2 3');
        INSERT INTO place_fts(rowid, name, detail) SELECT id, name, detail FROM place;
        INSERT INTO place_fts(place_fts) VALUES ('optimize');
        VACUUM;
    """)
    db.close()
    print(f"geocoder: done, {idx.n} rows in {out} ({time.time() - t0:.0f} s)")


if __name__ == "__main__":
    main()
