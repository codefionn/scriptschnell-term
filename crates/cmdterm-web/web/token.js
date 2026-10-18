// The --dev login page. The token is in the URL fragment, which the GET
// that fetched this page never sent to the server, so it stays out of
// server logs and browser history. POST it; the answer sets the session
// cookie, and the app starts. Without a token, go to the login page.

const out = document.getElementById('out');
const params = new URLSearchParams(location.hash.replace(/^#/, ''));
const token = params.get('token');

if (!token) {
  location.replace('/#login?error=that%20login%20link%20has%20no%20token');
} else {
  fetch('/api/auth/token', {
    method: 'POST',
    credentials: 'same-origin',
    headers: { 'Content-Type': 'application/json', Accept: 'application/json' },
    body: JSON.stringify({ token }),
  })
    .then(async (res) => {
      if (res.ok) {
        location.replace('/#machines');
      } else {
        const data = await res.json().catch(() => null);
        out.textContent = (data && data.error) || 'That login link is not valid.';
        // The token has failed; keep it out of the address bar and history.
        history.replaceState(null, '', location.pathname);
      }
    })
    .catch(() => {
      out.textContent = 'Cannot reach the server.';
    });
}
