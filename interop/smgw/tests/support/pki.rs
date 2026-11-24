//! Ephemeral HAN and WAN credentials in the layout expected by TlsEndpoint.
use openssl::{
    asn1::{Asn1Integer, Asn1Time},
    bn::{BigNum, MsbOption},
    ec::{EcGroup, EcKey},
    hash::MessageDigest,
    nid::Nid,
    pkey::{PKey, Private},
    x509::{
        extension::{BasicConstraints, ExtendedKeyUsage, KeyUsage, SubjectAlternativeName},
        X509NameBuilder, X509,
    },
};
use std::{fs, path::Path};

struct Identity {
    key: PKey<Private>,
    cert: X509,
}

enum Purpose {
    Authority(u32),
    Client,
    Server,
}

fn issue(name: &str, purpose: Purpose, issuer: Option<&Identity>) -> super::Result<Identity> {
    let group = EcGroup::from_curve_name(Nid::BRAINPOOL_P256R1)?;
    let key = PKey::from_ec_key(EcKey::generate(&group)?)?;
    let mut subject = X509NameBuilder::new()?;
    subject.append_entry_by_nid(Nid::COMMONNAME, name)?;
    let subject = subject.build();
    let mut serial = BigNum::new()?;
    serial.rand(128, MsbOption::ONE, false)?;
    let serial = Asn1Integer::from_bn(&serial)?;
    let mut cert = X509::builder()?;
    cert.set_version(2)?;
    cert.set_serial_number(&serial)?;
    cert.set_subject_name(&subject)?;
    cert.set_issuer_name(issuer.map_or(&subject, |ca| ca.cert.subject_name()))?;
    cert.set_pubkey(&key)?;
    cert.set_not_before(Asn1Time::days_from_now(0)?.as_ref())?;
    cert.set_not_after(Asn1Time::days_from_now(1)?.as_ref())?;
    match purpose {
        Purpose::Authority(depth) => {
            cert.append_extension(
                BasicConstraints::new()
                    .critical()
                    .ca()
                    .pathlen(depth)
                    .build()?,
            )?;
            cert.append_extension(
                KeyUsage::new()
                    .critical()
                    .key_cert_sign()
                    .crl_sign()
                    .build()?,
            )?;
        }
        Purpose::Client | Purpose::Server => {
            cert.append_extension(BasicConstraints::new().critical().build()?)?;
            cert.append_extension(KeyUsage::new().critical().digital_signature().build()?)?;
            let mut usage = ExtendedKeyUsage::new();
            match purpose {
                Purpose::Server => usage.server_auth(),
                _ => usage.client_auth(),
            };
            cert.append_extension(usage.build()?)?;
            let san = SubjectAlternativeName::new()
                .dns(name)
                .build(&cert.x509v3_context(issuer.map(|ca| ca.cert.as_ref()), None))?;
            cert.append_extension(san)?;
        }
    }
    cert.sign(issuer.map_or(&key, |ca| &ca.key), MessageDigest::sha256())?;
    Ok(Identity {
        key,
        cert: cert.build(),
    })
}

pub fn generate(dir: &Path) -> super::Result<()> {
    for (domain, client, server) in [("han", "cls", "smgw"), ("wan", "smgw", "aemt")] {
        let leg = dir.join(domain);
        fs::create_dir_all(&leg)?;
        let root = issue(&format!("{domain} test root"), Purpose::Authority(1), None)?;
        let intermediate = issue(
            &format!("{domain} test issuer"),
            Purpose::Authority(0),
            Some(&root),
        )?;
        fs::write(leg.join("root.pem"), root.cert.to_pem()?)?;
        for (role, purpose) in [(client, Purpose::Client), (server, Purpose::Server)] {
            let leaf = issue(&format!("{role}.test"), purpose, Some(&intermediate))?;
            let mut chain = leaf.cert.to_pem()?;
            chain.extend(intermediate.cert.to_pem()?);
            fs::write(leg.join(format!("{role}.chain.pem")), chain)?;
            let key = leg.join(format!("{role}.key.pem"));
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            std::io::Write::write_all(
                &mut options.open(key)?,
                &leaf.key.private_key_to_pem_pkcs8()?,
            )?;
        }
    }
    Ok(())
}
