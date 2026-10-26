use anyhow::{Context, Result, ensure};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

pub struct Pki {
    pub directory: tempfile::TempDir,
}

impl Pki {
    pub fn new() -> Result<Self> {
        let pki = Self {
            directory: tempfile::tempdir()?,
        };
        pki.ca("ca")?;
        pki.ca("other-ca")?;
        pki.leaf("server", "ca", "serverAuth", 1)?;
        for name in ["alpha", "beta", "gamma", "unregistered"] {
            pki.leaf(name, "ca", "clientAuth", 1)?;
        }
        pki.leaf("untrusted", "other-ca", "clientAuth", 1)?;
        pki.leaf("expired", "ca", "clientAuth", -1)?;
        Ok(pki)
    }

    pub fn path(&self, file: &str) -> PathBuf {
        self.directory.path().join(file)
    }
    pub fn pem(&self, name: &str) -> Result<String> {
        Ok(fs::read_to_string(self.path(&format!("{name}.pem")))?)
    }

    fn run(&self, args: &[&str]) -> Result<()> {
        let result = Command::new("openssl")
            .args(args)
            .current_dir(self.directory.path())
            .output()
            .context("OpenSSL is required for local TLS checks")?;
        ensure!(
            result.status.success(),
            "OpenSSL test certificate generation failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        Ok(())
    }

    fn ca(&self, name: &str) -> Result<()> {
        self.run(&[
            "req",
            "-x509",
            "-newkey",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:P-256",
            "-nodes",
            "-keyout",
            &format!("{name}.key"),
            "-out",
            &format!("{name}.pem"),
            "-days",
            "1",
            "-subj",
            &format!("/CN=CLEX test {name}"),
            "-addext",
            "basicConstraints=critical,CA:TRUE",
            "-addext",
            "keyUsage=critical,keyCertSign,cRLSign",
        ])
    }

    fn leaf(&self, name: &str, ca: &str, usage: &str, days: i32) -> Result<()> {
        self.run(&[
            "req",
            "-new",
            "-newkey",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:P-256",
            "-nodes",
            "-keyout",
            &format!("{name}.key"),
            "-out",
            &format!("{name}.csr"),
            "-subj",
            "/CN=certificate-user",
        ])?;
        let extensions = format!(
            "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage={usage}\nsubjectAltName=DNS:localhost,IP:127.0.0.1\n"
        );
        fs::write(self.path(&format!("{name}.ext")), extensions)?;
        if days < 0 {
            // openssl ca supports explicit historical validity across OpenSSL
            // releases; newer x509 commands reject negative -days values.
            fs::write(self.path("expired-index"), "")?;
            fs::write(self.path("expired-serial"), "01\n")?;
            fs::write(
                self.path("expired-ca.cnf"),
                format!(
                    "[ca]\ndefault_ca=issuer\n[issuer]\ndatabase=expired-index\nserial=expired-serial\nnew_certs_dir=.\ncertificate={ca}.pem\nprivate_key={ca}.key\ndefault_md=sha256\npolicy=names\nx509_extensions=client\n[names]\ncommonName=supplied\n[client]\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=clientAuth\n"
                ),
            )?;
            return self.run(&[
                "ca",
                "-batch",
                "-config",
                "expired-ca.cnf",
                "-in",
                &format!("{name}.csr"),
                "-out",
                &format!("{name}.pem"),
                "-notext",
                "-startdate",
                "200101000000Z",
                "-enddate",
                "200102000000Z",
            ]);
        }
        self.run(&[
            "x509",
            "-req",
            "-in",
            &format!("{name}.csr"),
            "-CA",
            &format!("{ca}.pem"),
            "-CAkey",
            &format!("{ca}.key"),
            "-set_serial",
            &format!("0x{}", uuid::Uuid::new_v4().simple()),
            "-days",
            &days.to_string(),
            "-extfile",
            &format!("{name}.ext"),
            "-out",
            &format!("{name}.pem"),
        ])
    }
}

pub fn certificates(path: &Path) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    Ok(
        rustls_pemfile::certs(&mut std::io::BufReader::new(fs::File::open(path)?))
            .collect::<std::io::Result<Vec<_>>>()?,
    )
}
