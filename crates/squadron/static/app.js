document.addEventListener('click', function (e) {
  var chip = e.target.closest('[data-brief]');
  if (chip) {
    var ta = document.querySelector('#brief');
    if (ta) ta.value = chip.getAttribute('data-brief');
  }
});
