#!/usr/bin/env bash
#
# session_survival_test.sh — does the CarPlay session SURVIVE?
#
# Runs the real sender (`rascar-carplay --carplay-endpoint`) against the local
# CarPlay receiver simulator and asserts the session lives well past the ~6 s
# mark where the Jaguar F-PACE tore it down, with video actually flowing.
#
# The simulator reproduces the car's fatal behaviour: if the sender hands
# MainAudio to the accessory in `modesChanged` and then SETUPs a MainAudio
# stream anyway, it stops answering and drops the link — exactly as the car does.
#
# Usage:  session_survival_test.sh [path-to-rascar-carplay]
set -u

BIN=${1:-$(dirname "$(readlink -f "$0")")/../../target/release/rascar-carplay}
SIM=$(dirname "$(readlink -f "$0")")/carplay_receiver_sim.py
OUT=$(mktemp -d)
PORT=${PORT:-7000}
RUN_SECS=${RUN_SECS:-18}
MIN_LIFE=${MIN_LIFE:-15}
MIN_FRAMES=${MIN_FRAMES:-100}

[[ -x $BIN ]] || { echo "no binary at $BIN"; exit 2; }

fail=0
# MEETING=1: rascar-app's meeting heartbeat ("call 1" to 127.0.0.1:7703)
# from 5 s to 11 s into the session, then "call 0".
# HANDBACK=1: "screen car" to 127.0.0.1:7704 at 6 s; the sim answers with
# requestUI 3 s after the hand-back and checks the whole transition.
# LATE_AUDIO=1: the sim answers the audio SETUP 3.2 s late; media audio must arrive.
run_case() {
    local label=$1; shift
    local meeting=0 handback=0 sim_extra=()
    if [[ ${1:-} == MEETING=1 ]]; then meeting=1; sim_extra=(--expect-meeting); shift; fi
    if [[ ${1:-} == HANDBACK=1 ]]; then handback=1; sim_extra+=(--expect-handback); shift; fi
    if [[ ${1:-} == LATE_AUDIO=1 ]]; then sim_extra+=(--audio-setup-delay 3.2 --expect-media-audio); shift; fi
    echo "── $label"
    timeout $((RUN_SECS + 20)) python3 "$SIM" --port "$PORT" \
        --run-secs "$RUN_SECS" --min-survival-secs "$MIN_LIFE" \
        --min-frames "$MIN_FRAMES" "${sim_extra[@]}" > "$OUT/$label.sim" 2>&1 &
    local sim=$!
    sleep 1.5
    env "$@" timeout $((RUN_SECS + 10)) "$BIN" --carplay-endpoint "127.0.0.1:$PORT" \
        > "$OUT/$label.snd" 2>&1 &
    if [[ $meeting -eq 1 ]]; then
        ( sleep 5; for _ in 1 2 3; do printf 'call 1' > /dev/udp/127.0.0.1/7703; sleep 2; done
          printf 'call 0' > /dev/udp/127.0.0.1/7703 ) &
    fi
    if [[ $handback -eq 1 ]]; then
        # rascar-app's "car screen" button (POST /api/carplay/native) sends this.
        ( sleep 6; printf 'screen car' > /dev/udp/127.0.0.1/7704 ) &
    fi
    wait $sim; local rc=$?
    grep -E "session lifetime|video frames|MainAudio entity|modesChanged after|audio trap fired|audio SETUPs|phoneCall entity|audio packets|screen timeline|screen handback|FAIL" "$OUT/$label.sim" | sed 's/^/    /'
    if [[ $rc -eq 0 ]]; then
        echo "    PASS"
    else
        echo "    FAIL (see $OUT/$label.sim and $OUT/$label.snd)"
        fail=1
    fi
    echo
}

echo "session survival gate — binary: $BIN"
echo
# Screen-only: we leave MainAudio to the car, so we must not ask for the stream.
run_case screen_only RASCAR_SESSION_GATE=1
# Audio claimed AND streamed — the other self-consistent configuration.
run_case audio_claimed RASCAR_CARPLAY_AUDIO=on
# A meeting mid-session: media -> telephony (car mic) -> media.
run_case meeting MEETING=1 RASCAR_CARPLAY_AUDIO=on
# Screen handed to the car's native UI at ~6 s, taken back on the car's requestUI.
run_case handback HANDBACK=1 RASCAR_CARPLAY_AUDIO=on
# The car answers the audio SETUP 3.2 s late (2026-10-01): audio must still flow.
run_case late_audio LATE_AUDIO=1 RASCAR_CARPLAY_AUDIO=on

if [[ $fail -eq 0 ]]; then
    echo "RESULT: session survives in every configuration."
else
    echo "RESULT: session did NOT survive."
fi
exit $fail
