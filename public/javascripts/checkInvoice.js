function queryAPI(rHash) {
    if (rHash) {
        fetch('api/status/' + rHash)
            .then(async response => {
                const text = await response.text();
                if (response.ok) {
                    window.location.href = '/success?rHash=' + encodeURIComponent(rHash);
                } else if (response.status === 400 && text) {
                    console.error('Bad Request: ' + text);
                } else {
                    console.error('Error:', response.status);
                }
            })
            .catch(error => {
                console.error(error);
            });
    } else {
        console.error('No rHash provided');
    }
}
