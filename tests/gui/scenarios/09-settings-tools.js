// Settings shows the tools for MariaDB servers and the tools for MySQL servers apart, each with its
// status and download, and marks the set the connected server uses.
(async () => {
  await openSettings();
  const status = $('cfgStatus');
  await G.until(() => /connected server is/.test(status.textContent) || /Could not tell/.test(status.textContent), 30000);
  const maria = $('cfgCardMaria'), mysql = $('cfgCardMysql');
  G.check('two cards, one for each kind of server', maria && mysql && /For MariaDB servers/.test(maria.innerText) && /For MySQL servers/.test(mysql.innerText), [maria && maria.innerText.slice(0, 40), mysql && mysql.innerText.slice(0, 40)]);
  G.check('each card has its own paths and download', maria.contains($('cfgMysql')) && maria.contains($('cfgDump')) && /Download MariaDB client tools/.test(maria.innerText)
    && mysql.contains($('cfgMysqlMy')) && mysql.contains($('cfgDumpMy')) && /Download MySQL client tools/.test(mysql.innerText), 'fields or buttons in the wrong card');
  G.check('the MariaDB card shows its tools', /mysql:/.test($('cfgStatusMaria').innerText) && /mysqldump:/.test($('cfgStatusMaria').innerText), $('cfgStatusMaria').innerText);
  const serverIsMaria = /MariaDB/i.test(await G.one('SELECT VERSION()'));
  const mysqlTools = !/none - the tools for MariaDB servers are used/.test($('cfgStatusMysql').innerText);
  const inUse = (!serverIsMaria && mysqlTools) ? mysql : maria;
  G.check('the card the connected server uses is marked, and only that one', inUse.classList.contains('inuse') && !(inUse === maria ? mysql : maria).classList.contains('inuse'),
    { status: status.textContent, maria: maria.className, mysql: mysql.className });
  G.check('and the line above says so', new RegExp('connected server is ' + (serverIsMaria ? 'MariaDB' : 'MySQL')).test(status.textContent), status.textContent);
  hide('mSettings');
  return G.report();
})()
