// The new-version notice, with GitHub's answer stubbed: shown when newer, gone when hidden, and
// the switch in Settings.
(async () => {
  const realApi = api;
  api = async (p, b) => p === '/api/update-check'
    ? { ok: true, current: '1.0.0', latest: '9.9.9', newer: true, url: 'https://github.com/monsama/NOBS-SQL-Editor/releases/tag/v9.9.9' }
    : realApi(p, b);
  try {
    try { localStorage.removeItem('updateDismissed'); } catch (e) { }
    const note = $('updNote');
    const barBefore = $('bar').getBoundingClientRect().height;
    await checkForUpdate(false);
    G.check('a newer release is announced', note.style.display !== 'none' && /9\.9\.9/.test(note.innerText), note.innerText);
    G.eq('without changing the height of the top bar', $('bar').getBoundingClientRect().height, barBefore);
    dismissUpdate();
    G.eq('hiding it hides it', note.style.display, 'none');
    await checkForUpdate(false);
    G.eq('and that version stays hidden', note.style.display, 'none');
    await openSettings();
    G.check('Settings has the switch', !!$('cfgUpdateCheck'), 'no switch');
    hide('mSettings');
    if (G.desktop) {
      const r = await realApi('/api/open-release-page', { url: 'https://github.com/monsama/NOBS-SQL-Editor/releases/tag/v1.2.0&calc' });
      G.check('the desktop app opens only its own release pages', !r.ok, r);
    }
  } finally {
    api = realApi;
    try { localStorage.removeItem('updateDismissed'); } catch (e) { }
  }
  return G.report();
})()
