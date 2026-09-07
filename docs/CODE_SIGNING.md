# Code signing

This document explains how the release pipeline signs Windows binaries, which certificate
authorities an India-based individual developer can actually buy from, and which steps
require a human because they involve identity verification or payment.

It is written to be reusable: nothing here is specific to Codenotch, so the same certificate
and the same `scripts/sign.ps1` can sign any future Windows application.

## Why this matters

An unsigned Windows executable downloaded from the internet triggers Microsoft Defender
SmartScreen: "Windows protected your PC", with the Run button hidden behind "More info".
Most users stop there. Signing does not remove that warning on day one — it attaches a
verified publisher identity to the binary so that SmartScreen can start accumulating
reputation for that identity across every download.

Two consequences follow, and both need to be said plainly rather than discovered later:

- **An OV or IV certificate does not clear SmartScreen immediately.** Reputation builds
  over downloads and time. Early users will still see the warning.
- **An EV certificate does clear it immediately**, because the identity vetting is
  stricter. That is the only real difference in day-one user experience, and it is why EV
  costs several times more.

## What the pipeline already does

`.github/workflows/release.yml` calls `scripts/sign.ps1` for the application binary and for
the NSIS installer, before checksums are computed. `sign.ps1` reads `CODESIGN_PROVIDER`
from the environment:

- unset - the script logs that it is skipping and exits 0, so unsigned builds still work
  on forks and on pull requests from contributors who have no access to the secrets;
- set to a provider it knows - it asserts that the provider's other secrets are present and
  signs;
- set to something unknown, or set with missing secrets - it fails the build.

That last case is deliberate. A signing step that silently skips on misconfiguration
publishes an unsigned installer while the release notes claim it is signed.

Turning signing on is therefore a change to repository secrets plus one implementation
function, not a rewrite of the workflow.

### Secrets the workflow reads

| Secret | Meaning |
| --- | --- |
| `CODESIGN_PROVIDER` | `sslcom`, `digicert` or `azure`. Unset disables signing. |
| `CODESIGN_USERNAME` | Portal account username (SSL.com eSigner). |
| `CODESIGN_PASSWORD` | Portal account password. |
| `CODESIGN_CREDENTIAL_ID` | Identifies which key in the cloud HSM to sign with. |
| `CODESIGN_TOTP_SECRET` | The TOTP seed that lets CI complete two-factor signing without a human. |
| `CODESIGN_TIMESTAMP_URL` | Optional. Defaults to `http://ts.ssl.com`. |

Every signature is RFC 3161 timestamped. Without a timestamp the signature stops
validating the day the certificate expires; with one it remains valid afterwards.

## Choosing a certificate authority

### Azure Trusted Signing is not an option from India

Microsoft's Trusted Signing (renamed Azure Artifact Signing in 2026) is the cheapest and
best-integrated option where it is available, so it is worth ruling out explicitly rather
than by omission.

- **Individual developers must be located in the United States or Canada.** This has not
  changed as of September 2026, and Microsoft's own Q&A threads carry repeated
  "my country is not available" reports from individuals elsewhere.
- Organizations get wider coverage - the US, Canada, the EU, the UK, Australia, New
  Zealand, Japan, South Korea, Singapore, Switzerland, Norway and Israel - but India is not
  in that list either.
- Microsoft dropped the earlier three-years-of-business-history rule and opened sign-up to
  self-employed individuals, but only inside the same country list. Individual onboarding
  has additionally been reported as paused at points during 2026.

Re-check this before committing anywhere else; Microsoft has stated an intention to widen
availability, and it would be the cheapest path the moment India is included.

### The three realistic options

All three hold the private key in a CA-operated cloud HSM. Since June 2023 the CA/Browser
Forum has required code-signing private keys to live on certified hardware, so a
downloadable `.pfx` file is no longer issued by any public CA. Cloud HSM is what makes
unattended CI signing possible at all - the alternative is a physical USB token, which
cannot be plugged into a GitHub-hosted runner.

Note a 2026 change that affects all of them: publicly trusted code-signing certificates are
now issued for a maximum of one year (DigiCert moved to one-year plans only from
15 February 2026, and the wider 460-day cap took effect 1 March 2026). Treat every price
below as recurring annually, not one-time.

#### 1. Certum Open Source Code Signing - cheapest, fits this project

- Roughly **$100-130 per year**, the lowest publicly trusted option available.
- Explicitly sold to **individuals**, worldwide, including India. Delivered through
  Certum's SimplySign cloud, so no hardware token.
- **The catch:** the publisher line is fixed to `Open Source Developer, <Your Name>` rather
  than your name alone, and the certificate may not be used to sign commercially
  distributed software.
- Verification needs a government ID check (automated against ID databases, at a
  registration point, notarized, or a photo-holding-ID submission), a recent utility bill
  or bank statement proving your address, and a link to the open-source project.

For Codenotch specifically this is a good fit: the project is MIT and not sold. For a
future commercial application it is not usable, and you would need one of the options
below.

#### 2. SSL.com IV (Individual Validation) via eSigner - best CI story

- Certificate plus eSigner cloud signing, from roughly **$240 per year** at the entry tier
  (20 signings per month); higher tiers scale to 1,000 signings per month. Annual billing
  is about 25% cheaper than monthly, and new subscriptions include 30 days of unlimited
  signing.
- SSL.com is a US-based CA that issues Individual Validation certificates to individuals
  internationally. The publisher line is your own legal name, with no open-source-only
  restriction.
- eSigner's `CodeSignTool` is the most mature CLI for unattended CI signing, which is why
  `sign.ps1` names it first and why the `CODESIGN_TOTP_SECRET` secret exists.

#### 3. Sectigo / Comodo individual OV through a reseller - middle ground

- Roughly **$215-220 per year** through resellers such as SignMyCode or SSL Dragon.
- Sold to individual developers, cloud signing available. No open-source restriction.
- Reseller-mediated, so support quality varies and the signing CLI is less well documented
  than SSL.com's.

### Recommendation

**Start with Certum Open Source Code Signing** for Codenotch. It is the cheapest publicly
trusted certificate, it is explicitly available to individuals in India, and this project
satisfies its one real constraint by being open source and non-commercial. The
`Open Source Developer` publisher prefix is cosmetic here.

Move to **SSL.com IV** when you first ship something commercial, or if the publisher line
matters to you. Consider **EV** only when SmartScreen warnings on day one become a real
support burden - it is the only thing that removes them immediately.

## What only you can do

These steps involve payment, identity documents, or a portal login. They cannot be
automated or done with elevated local access.

1. **Choose a CA and pay.** Certum sells directly (`certum.store`) and through resellers
   such as SSLmentor; SSL.com sells directly.
2. **Prove your identity.** Have ready: a government photo ID (passport or Aadhaar/PAN,
   depending on what the CA accepts for India), and a recent utility bill or bank statement
   in your name showing your address, dated within the last three to six months. Some CAs
   additionally require a short video call or a notarized document. Names must match
   exactly across every document - a mismatch is the most common cause of a rejected
   application.
3. **For Certum specifically, supply a link to the open-source project** the certificate is
   for. `https://github.com/DhakadG/codenotch-windows` serves for this.
4. **Enrol the key in the CA's cloud HSM** through their portal, and record the credential
   ID it gives you.
5. **Enable TOTP for automated signing** and save the TOTP *seed* (the base32 string behind
   the QR code), not just the app entry. CI needs the seed to generate codes without you.
6. **Add the repository secrets** at Settings - Secrets and variables - Actions on
   `DhakadG/codenotch-windows`: `CODESIGN_PROVIDER`, `CODESIGN_USERNAME`,
   `CODESIGN_PASSWORD`, `CODESIGN_CREDENTIAL_ID`, `CODESIGN_TOTP_SECRET`.

Once step 6 is done, tell me which provider you bought and I will fill in the corresponding
branch of `scripts/sign.ps1`, which is currently a documented `throw` rather than a silent
no-op for exactly that reason.

## Verifying a signature

After a signed release build:

```powershell
Get-AuthenticodeSignature .\Codenotch_0.3.0_x64-setup.exe | Format-List Status, StatusMessage, SignerCertificate, TimeStamperCertificate
```

`Status` must be `Valid`, and `TimeStamperCertificate` must not be empty. An empty
timestamper means the signature will fail validation once the certificate expires.

## Sources

Verified September 2026. Certificate pricing and eligibility change often - re-check before
paying.

- <https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/code-signing-options>
- <https://learn.microsoft.com/en-us/answers/questions/5810735/cant-create-a-new-trusted-signing-individual-ident>
- <https://azure.microsoft.com/en-us/products/artifact-signing>
- <https://www.ssl.com/guide/esigner-pricing-for-code-signing/>
- <https://www.ssl.com/products/software-integrity/signing-service/>
- <https://certum.store/standard-code-signing-in-the-cloud.html>
- <https://www.sslmentor.com/certum/certumcodecloudindividual>
- <https://signmycode.com/individual-code-signing>
