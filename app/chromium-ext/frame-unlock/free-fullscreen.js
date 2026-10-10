// Free TV (Oqee) has no double-click-to-fullscreen, and in the car there is
// no fullscreen button to reach either. Double-click the video to fill the
// screen, double-click again to come back — like YouTube and Netflix do.
document.addEventListener('dblclick', e => {
  if (document.fullscreenElement) {
    document.exitFullscreen().catch(() => {});
    return;
  }
  const video = [...document.querySelectorAll('video')].find(v => {
    const r = v.getBoundingClientRect();
    return e.clientX >= r.left && e.clientX <= r.right && e.clientY >= r.top && e.clientY <= r.bottom;
  });
  if (!video) return;
  // The player's box, so its own controls and subtitles come along.
  const player = video.closest('[class*="player" i]') || video.parentElement || video;
  player.requestFullscreen().catch(() => {});
}, true);
