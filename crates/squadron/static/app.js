document.addEventListener('click', function (e) {
  var chip = e.target.closest('[data-brief]');
  if (chip) {
    var ta = document.querySelector('#brief');
    if (ta) ta.value = chip.getAttribute('data-brief');
    return;
  }
  // F25: the entire runs-table row is clickable (the run id column is
  // hidden); inner links still win so the arrow/actions stay precise.
  var row = e.target.closest('tr[data-run-link]');
  if (row && !e.target.closest('a')) {
    window.location.href = row.getAttribute('data-run-link');
  }
});

// Keyboard access for the clickable rows.
document.addEventListener('keydown', function (e) {
  if (e.key !== 'Enter' && e.key !== ' ') return;
  var row = e.target.closest('tr[data-run-link]');
  if (row) {
    e.preventDefault();
    window.location.href = row.getAttribute('data-run-link');
  }
});
