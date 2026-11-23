import { mount } from 'svelte';
import './app.css';
import Gallery from './Gallery.svelte';
import LaunchDialog from './LaunchDialog.svelte';

const el = document.getElementById('island');
if (el) {
  el.innerHTML = '';
  mount(Gallery, {
    target: el,
    props: {
      runId: el.dataset.runId,
      eventsUrl: el.dataset.eventsUrl,
      csrfToken: el.dataset.csrfToken
    }
  });
}

const launch = document.getElementById('launch-island');
if (launch) {
  launch.innerHTML = '';
  mount(LaunchDialog, {
    target: launch,
    props: {
      csrfToken: launch.dataset.csrfToken
    }
  });
}
