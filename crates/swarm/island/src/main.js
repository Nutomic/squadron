import { mount } from 'svelte';
import './app.css';
import Gallery from './Gallery.svelte';

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
