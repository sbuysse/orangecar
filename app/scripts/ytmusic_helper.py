#!/usr/bin/env python3
"""RasCar YouTube Music sidecar.

Thin CLI wrapper around `ytmusicapi` (the de-facto unofficial YouTube Music
client). The Rust `MusicService` shells out to this script; it always prints a
single JSON object to stdout and never hangs.

Invocation:
    python3 ytmusic_helper.py <command> [args...]

Commands:
    status                        -> {"connected": true}
    home [shelves]                -> {"shelves": [{"title": "...", "items": [...]}]}
    history [limit]               -> {"tracks": [...]}
    liked [limit]                 -> {"tracks": [...]}
    playlists                     -> {"playlists": [...]}
    playlist <id>                 -> {"title": "...", "tracks": [...]}
    search <query>                -> {"tracks": [...], "artists": [...]}
    search_one <title> <artist>   -> {"videoId": "..."}
    rate <videoId> <RATING>       -> {"ok": true}   (RATING: LIKE|DISLIKE|INDIFFERENT)

No credentials live here. With $RASCAR_YTM_BRIDGE set (the dashboard does it
unless the user signed out), every API call is sent to that loopback endpoint
of rascar-app, which runs it as a fetch() inside a music.youtube.com tab of the
car browser (services/ytbridge.rs): the browser adds its own cookies and the
page its own SAPISIDHASH, so the Google session never leaves the browser
(copying its cookies here got the session revoked within the hour,
2026-09-29). ytmusicapi is given placeholder browser-auth headers only so that
it treats itself as signed in; they are dropped before the call leaves.
Without the bridge, public commands run anonymously and directly. OAuth tokens
are not an option: YouTube Music's API answers them "HTTP 400: Request contains
an invalid argument" (ytmusicapi 1.12.3, 2026-09-29).

On any failure (ytmusicapi missing, not signed in, no match, API error) it prints {"error": "<reason>", "code": "<category>"} to stdout and exits
non-zero. `code` is one of auth / unavailable / upstream / notfound; the caller
logs `error` server-side and shows only `code` to HTTP clients, because `error`
can contain tracebacks and absolute paths.

Every network call is bounded: `ytmusicapi` uses `requests` with no timeout of
its own, so a half-open TCP connection would block forever. A socket default
timeout covers the normal case and SIGALRM is the hard backstop.
"""

import json
import os
import signal
import socket
import sys

# Per-socket timeout for the HTTP calls ytmusicapi makes. urllib3 falls back to
# the socket default when requests passes no explicit timeout.
SOCKET_TIMEOUT_SECS = 10
# Hard wall-clock ceiling for the whole invocation. Must stay below the caller's
# own timeout (MUSIC_TIMEOUT, 20s) so we emit proper JSON instead of being
# killed mid-request.
HARD_TIMEOUT_SECS = 17


def _fail(reason, code="upstream"):
    """Print an error object and exit non-zero. Never raises."""
    try:
        print(json.dumps({"error": str(reason), "code": code}))
    except Exception:
        print('{"error": "unknown error", "code": "upstream"}')
    sys.exit(1)


def _on_alarm(signum, frame):
    _fail("timed out after %ds" % HARD_TIMEOUT_SECS, "upstream")


def _install_timeouts():
    """Bound every network call, and the invocation as a whole."""
    socket.setdefaulttimeout(SOCKET_TIMEOUT_SECS)
    # SIGALRM is POSIX-only; on anything else the socket timeout stands alone.
    if hasattr(signal, "SIGALRM"):
        signal.signal(signal.SIGALRM, _on_alarm)
        signal.alarm(HARD_TIMEOUT_SECS)


# Placeholder browser-auth headers: ytmusicapi requires a __Secure-3PAPISID
# cookie and a SAPISIDHASH authorization to run as a signed-in client. None of
# it reaches YouTube: the bridge scrubs identity headers and the page sets its
# own. The visitor id is present so ytmusicapi does not fetch the home page
# for it (the page's own is used).
BRIDGE_AUTH = {
    "cookie": "SAPISID=bridge; __Secure-3PAPISID=bridge",
    "authorization": "SAPISIDHASH 0_bridge",
    "x-goog-authuser": "0",
    "x-goog-visitor-id": "",
    "content-type": "application/json",
    "accept": "*/*",
    "origin": "https://music.youtube.com",
}
# Never forwarded by the adapter (the bridge drops them too).
_IDENTITY_HEADERS = ("cookie", "authorization", "x-goog-authuser", "x-goog-visitor-id", "origin", "x-origin")


def _bridge_session_class():
    """`requests.Session` whose every request goes through the browser bridge.
    Built lazily so the helper still answers cleanly without `requests`."""
    import requests
    from requests.structures import CaseInsensitiveDict

    class BridgeSession(requests.Session):
        # Set by the last reply: whether the browser tab holds a Google session.
        signed_in = None

        def __init__(self, bridge_url, token, timeout=SOCKET_TIMEOUT_SECS + 5):
            super().__init__()
            self.bridge_url = bridge_url
            self.token = token
            self.bridge_timeout = timeout

        def request(self, method, url, params=None, data=None, headers=None, cookies=None,
                    files=None, auth=None, timeout=None, allow_redirects=True, proxies=None,
                    hooks=None, stream=None, verify=None, cert=None, json=None):
            # Let requests build the final URL and body; cookies are ignored
            # on purpose (the browser sends its own).
            prepared = requests.Request(method.upper(), url, params=params, data=data, json=json,
                                        headers={k: v for k, v in (headers or {}).items()
                                                 if k.lower() not in _IDENTITY_HEADERS}).prepare()
            body = prepared.body
            if isinstance(body, bytes):
                body = body.decode("utf-8")
            payload = {"method": prepared.method, "url": prepared.url,
                       "headers": dict(prepared.headers), "body": body}
            payload["headers"].pop("Content-Length", None)
            reply = requests.post(self.bridge_url, json=payload, timeout=self.bridge_timeout,
                                  headers={"x-rascar-bridge": self.token})
            if reply.status_code != 200:
                raise requests.exceptions.ConnectionError(
                    "browser bridge answered HTTP %d" % reply.status_code)
            answer = reply.json()
            BridgeSession.signed_in = bool(answer.get("signedIn"))
            resp = requests.Response()
            resp.status_code = int(answer.get("status", 0))
            resp.reason = answer.get("statusText", "") or ""
            resp.headers = CaseInsensitiveDict(answer.get("headers") or {})
            resp._content = (answer.get("body") or "").encode("utf-8")
            resp.encoding = "utf-8"
            resp.url = prepared.url
            resp.request = prepared
            return resp

    return BridgeSession


# Session class of an account command run over the bridge (see main): its
# answer only counts when the browser was signed in. Signed out, YouTube Music
# answers library calls with empty lists instead of an error.
_ACCOUNT_SESSION = None


def _emit(obj):
    """Print exactly one JSON object to stdout and exit 0."""
    if _ACCOUNT_SESSION is not None and _ACCOUNT_SESSION.signed_in is False:
        _fail("the car browser is not signed in to YouTube Music", "auth")
    print(json.dumps(obj))
    sys.exit(0)


def _art(item):
    """Return the largest thumbnail URL from an item, or '' if none.

    Thumbnails are usually a list of {url,width,height} ordered small->large,
    so the last entry is the largest. Guard every level defensively.
    """
    if not isinstance(item, dict):
        return ""
    thumbs = item.get("thumbnails")
    # get_playlist nests them under item['thumbnails']; some responses nest one
    # level deeper (e.g. {'thumbnail': {'thumbnails': [...]}}).
    if not thumbs and isinstance(item.get("thumbnail"), dict):
        thumbs = item["thumbnail"].get("thumbnails")
    # get_watch_playlist (radio) puts the list directly under 'thumbnail'.
    if not thumbs and isinstance(item.get("thumbnail"), list):
        thumbs = item["thumbnail"]
    if isinstance(thumbs, list) and thumbs:
        last = thumbs[-1]
        if isinstance(last, dict):
            return last.get("url", "") or ""
    return ""


def _artists(item):
    """Join artist names into a single string, defensively."""
    if not isinstance(item, dict):
        return ""
    arts = item.get("artists")
    if isinstance(arts, list):
        names = [a.get("name", "") for a in arts if isinstance(a, dict) and a.get("name")]
        if names:
            return ", ".join(names)
    # Some result types put the name directly under 'artist'.
    if isinstance(item.get("artist"), str):
        return item["artist"]
    return ""


def _album(item):
    """Extract an album name from a track item, defensively."""
    if not isinstance(item, dict):
        return ""
    album = item.get("album")
    if isinstance(album, dict):
        return album.get("name", "") or ""
    if isinstance(album, str):
        return album
    return ""


def _track(item):
    """Normalize a track/song item to our fixed shape."""
    if not isinstance(item, dict):
        return {"videoId": "", "title": "", "artist": "", "album": "", "art": ""}
    return {
        "videoId": item.get("videoId", "") or "",
        "title": item.get("title", "") or "",
        "artist": _artists(item),
        "album": _album(item),
        "art": _art(item),
    }


def _tracks(items):
    if not isinstance(items, list):
        return []
    return [_track(i) for i in items]


def cmd_status(yt, args):
    # The account menu only exists for a logged-in session. (Library calls do
    # NOT prove it: signed out they answer an empty list, not an error.)
    yt.get_account_info()
    _emit({"connected": True})


def _item(entry):
    """A home-shelf entry: a playable song/video, or a playlist/album to open."""
    if not isinstance(entry, dict):
        return None
    base = {"title": entry.get("title", "") or "", "subtitle": _artists(entry) or entry.get("description", "") or "",
            "art": _art(entry)}
    if entry.get("videoId"):
        base.update(kind="track", videoId=entry["videoId"], artist=_artists(entry), album=_album(entry))
        return base
    if entry.get("playlistId"):
        base.update(kind="playlist", id=entry["playlistId"])
        return base
    return None   # artists, albums without a playlist id: not openable here


def cmd_home(yt, args):
    """Home feed shelves ("Quick picks", "Listen again", mixes …). Public data
    when signed out, personalised when signed in."""
    limit = 6
    if args:
        try:
            limit = max(1, min(12, int(args[0])))
        except (ValueError, TypeError):
            limit = 6
    shelves = []
    for shelf in yt.get_home(limit=limit) or []:
        if not isinstance(shelf, dict):
            continue
        items = [i for i in (_item(e) for e in shelf.get("contents") or []) if i]
        if items:
            shelves.append({"title": shelf.get("title", "") or "", "items": items[:20]})
    _emit({"shelves": shelves})


def cmd_history(yt, args):
    limit = 30
    if args:
        try:
            limit = max(1, min(200, int(args[0])))
        except (ValueError, TypeError):
            limit = 30
    tracks = [t for t in _tracks(yt.get_history() or []) if t["videoId"]]
    # The history repeats a song each time it was played: keep the latest play.
    seen, out = set(), []
    for t in tracks:
        if t["videoId"] not in seen:
            seen.add(t["videoId"])
            out.append(t)
    _emit({"tracks": out[:limit]})


def cmd_liked(yt, args):
    limit = 100
    if args:
        try:
            limit = max(1, min(1000, int(args[0])))
        except (ValueError, TypeError):
            limit = 100
    result = yt.get_liked_songs(limit=limit)
    tracks = result.get("tracks", []) if isinstance(result, dict) else []
    _emit({"tracks": _tracks(tracks)})


def cmd_playlists(yt, args):
    result = yt.get_library_playlists(limit=100)
    playlists = []
    if isinstance(result, list):
        for p in result:
            if not isinstance(p, dict):
                continue
            playlists.append({
                "id": p.get("playlistId", "") or "",
                "title": p.get("title", "") or "",
                "count": p.get("count", "") or "",
                "art": _art(p),
            })
    _emit({"playlists": playlists})


def cmd_playlist(yt, args):
    if not args:
        _fail("playlist command requires an id", "notfound")
    limit = 200
    if len(args) > 1:
        try:
            limit = max(1, min(2000, int(args[1])))
        except (ValueError, TypeError):
            limit = 200
    result = yt.get_playlist(args[0], limit=limit)
    if not isinstance(result, dict):
        _emit({"title": "", "tracks": []})
    _emit({
        "title": result.get("title", "") or "",
        "tracks": _tracks(result.get("tracks", [])),
    })


def cmd_search(yt, args):
    if not args:
        _fail("search command requires a query", "notfound")
    query = args[0]
    songs = yt.search(query, filter="songs", limit=20)
    artists_raw = yt.search(query, filter="artists", limit=10)
    artists = []
    if isinstance(artists_raw, list):
        for a in artists_raw:
            if not isinstance(a, dict):
                continue
            artists.append({
                "name": a.get("artist", "") or "",
                "id": a.get("browseId", "") or "",
                "art": _art(a),
            })
    _emit({"tracks": _tracks(songs), "artists": artists})


def cmd_search_one(yt, args):
    if len(args) < 1:
        _fail("search_one requires a title", "notfound")
    title = args[0]
    artist = args[1] if len(args) > 1 else ""
    query = (title + " " + artist).strip()
    songs = yt.search(query, filter="songs", limit=1)
    if isinstance(songs, list) and songs and isinstance(songs[0], dict):
        vid = songs[0].get("videoId")
        if vid:
            _emit({"videoId": vid})
    _fail("no matching track found", "notfound")


def cmd_rate(yt, args):
    if len(args) < 2:
        _fail("rate requires <videoId> <LIKE|DISLIKE|INDIFFERENT>", "notfound")
    video_id = args[0]
    rating = args[1].upper()
    if rating not in ("LIKE", "DISLIKE", "INDIFFERENT"):
        _fail("rating must be LIKE, DISLIKE or INDIFFERENT", "notfound")
    yt.rate_song(video_id, rating)
    _emit({"ok": True})


def cmd_artist(yt, args):
    """An artist's page: name, picture and top songs (public data)."""
    if not args:
        _fail("artist command requires a browseId", "notfound")
    a = yt.get_artist(args[0])
    songs = a.get("songs", {}) if isinstance(a, dict) else {}
    results = songs.get("results", []) if isinstance(songs, dict) else []
    _emit({
        "name": (a.get("name", "") if isinstance(a, dict) else "") or "",
        "art": _art(a),
        "tracks": [t for t in _tracks(results) if t["videoId"]],
    })


def cmd_lyrics(yt, args):
    """YouTube Music's own lyrics for a track, timestamped when available."""
    if not args:
        _fail("lyrics command requires a videoId", "notfound")
    wp = yt.get_watch_playlist(videoId=args[0], limit=1)
    browse = wp.get("lyrics") if isinstance(wp, dict) else None
    if not browse:
        _emit({"lines": [], "synced": False})
    lyr = yt.get_lyrics(browse, timestamps=True) or {}
    body = lyr.get("lyrics")
    if lyr.get("hasTimestamps") and isinstance(body, list):
        lines = [{"time_sec": (getattr(l, "start_time", 0) or 0) / 1000.0, "text": getattr(l, "text", "") or ""}
                 for l in body if (getattr(l, "text", "") or "").strip()]
        _emit({"lines": lines, "synced": True, "source": "YouTube Music"})
    text = body if isinstance(body, str) else ""
    lines = [{"time_sec": None, "text": t.strip()} for t in text.splitlines() if t.strip()]
    _emit({"lines": lines, "synced": False, "source": "YouTube Music"})


def cmd_radio(yt, args):
    """The radio (endless mix) YouTube Music builds from a track: what its own
    app plays after a song. The seed track comes first; callers drop it."""
    if not args:
        _fail("radio command requires a videoId", "notfound")
    result = yt.get_watch_playlist(videoId=args[0], radio=True, limit=30)
    tracks = result.get("tracks", []) if isinstance(result, dict) else []
    _emit({"tracks": [t for t in _tracks(tracks) if t["videoId"]]})


COMMANDS = {
    "status": cmd_status,
    "home": cmd_home,
    "history": cmd_history,
    "liked": cmd_liked,
    "playlists": cmd_playlists,
    "playlist": cmd_playlist,
    "search": cmd_search,
    "search_one": cmd_search_one,
    "rate": cmd_rate,
    "radio": cmd_radio,
    "artist": cmd_artist,
    "lyrics": cmd_lyrics,
}


def main():
    global _ACCOUNT_SESSION
    _install_timeouts()

    if len(sys.argv) < 2:
        _fail("usage: ytmusic_helper.py <command> [args...]", "unavailable")

    command = sys.argv[1]
    args = sys.argv[2:]

    handler = COMMANDS.get(command)
    # Commands that only read public catalogue data (no account needed).
    ANONYMOUS_COMMANDS = ("search", "search_one", "radio", "artist", "lyrics", "home", "playlist")
    if handler is None:
        _fail("unknown command: " + command, "unavailable")

    try:
        from ytmusicapi import YTMusic
    except Exception:
        _fail("ytmusicapi not installed (pip install ytmusicapi)", "unavailable")

    bridge = os.environ.get("RASCAR_YTM_BRIDGE", "")
    session_cls = None
    if bridge:
        try:
            session_cls = _bridge_session_class()
            yt = YTMusic(auth=dict(BRIDGE_AUTH), requests_session=session_cls(
                bridge, os.environ.get("RASCAR_YTM_BRIDGE_TOKEN", "")))
        except SystemExit:
            raise
        except Exception as e:
            _fail("could not initialize YTMusic over the browser bridge: " + str(e), "unavailable")
    elif command in ANONYMOUS_COMMANDS:
        # Search is public: without a sign-in, search and play still work.
        yt = YTMusic()
    else:
        _fail("signed out on the dashboard", "auth")
    if session_cls is not None and command not in ANONYMOUS_COMMANDS:
        _ACCOUNT_SESSION = session_cls

    try:
        handler(yt, args)
    except SystemExit:
        raise
    except Exception as e:
        import requests
        if isinstance(e, (requests.exceptions.ConnectionError, requests.exceptions.Timeout, socket.timeout)):
            _fail("browser bridge unreachable: " + str(e)[:200], "unavailable")
        if _ACCOUNT_SESSION is not None and _ACCOUNT_SESSION.signed_in is False:
            _fail("the car browser is not signed in to YouTube Music", "auth")
        _fail(str(e), "upstream")


if __name__ == "__main__":
    try:
        main()
    except SystemExit:
        raise
    except Exception as e:  # last-resort guard: never hang, always emit JSON
        _fail(str(e), "upstream")
