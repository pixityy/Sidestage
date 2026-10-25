# sidestage.it.com

The Sidestage website: plain static HTML, CSS and JavaScript modules with self-hosted fonts. It
has no build step, no tracking, no cookies and no third-party requests.

| Path | What |
|---|---|
| `index.html` | The page: hero, sound check, the cue sheet of features, proof (screenshot, demo video, stills), downloads, questions |
| `css/site.css` | All styles. Colours are the app's dark palette from `crates/sidestage/src/ui/theme.rs` |
| `js/site.js` | The radar (the same geometry as the app's `radar.rs`), the hero's entrance and idle motion, the sound check's radar and controls, reveals and the download highlight |
| `js/audio.js` | The sound check's sound, made with Web Audio: a music loop and a voice through HRTF panners, game footsteps played straight |
| `fonts/` | Bricolage Grotesque, Onest and Martian Mono (latin subsets) with their OFL licences |
| `404.html`, `favicon.svg`, `CNAME` | Pages extras |
| `serve.py` | Local preview server; not published |

## Preview locally

```bash
python website/serve.py
```

Then open <http://127.0.0.1:8770/>. The script serves `website/` at `/` and `docs/media/` at
`/media/`, the same layout the deploy workflow builds.

## Next update or released

`<html lang="en" data-update="...">` in `index.html` picks between two wordings: `next` shows the
multi-source update as coming, `out` (the setting since 0.2.0) as released. Elements with the
classes `if-next` and `if-out` hold the two versions of a sentence or tag.

## How it is published

`.github/workflows/pages.yml` runs on every push to `main` that touches `website/`, `docs/media/` or
the workflow itself, and by hand from the Actions tab. It copies `website/` into `_site/`, adds the
screenshots, poster and demo video from `docs/media/` under `_site/media/`, and deploys that to
GitHub Pages. The media files live only once in the repository, in `docs/media/`.

## Going live (one-time setup, done by the repository owner)

1. **Repository settings.** Settings > Pages > Build and deployment > Source: **GitHub Actions**.
2. **Run the workflow** once: push to `main`, or Actions > Website > Run workflow.
3. **DNS at the registrar for sidestage.it.com.** `it.com` is on the Public Suffix List, so
   `sidestage.it.com` is an apex domain: use A and AAAA records at the root, not a CNAME. Remove
   the parking or URL-redirect records the registrar added (the name currently points at a parking
   page), then create:

   | Type | Host | Value |
   |---|---|---|
   | A | `@` | `185.199.108.153` |
   | A | `@` | `185.199.109.153` |
   | A | `@` | `185.199.110.153` |
   | A | `@` | `185.199.111.153` |
   | AAAA | `@` | `2606:50c0:8000::153` |
   | AAAA | `@` | `2606:50c0:8001::153` |
   | AAAA | `@` | `2606:50c0:8002::153` |
   | AAAA | `@` | `2606:50c0:8003::153` |
   | CNAME | `www` | `pixityy.github.io` |

4. **Custom domain.** Settings > Pages > Custom domain: `sidestage.it.com`, then Save. With a
   workflow deployment GitHub uses this setting, not the `CNAME` file (which is kept for clarity
   and in case the site is ever deployed from a branch). Wait for the DNS check to pass, then tick
   **Enforce HTTPS**. The certificate can take up to an hour after DNS resolves.
5. **Recommended: verify the domain**, so nobody else can claim it for their Pages site. In your
   GitHub profile: Settings > Pages > Add a domain, then add the TXT record it shows
   (`_github-pages-challenge-pixityy`) at the registrar.

Before saving the custom domain, check the records with `nslookup sidestage.it.com`. It should list
the four `185.199.x.153` addresses.
