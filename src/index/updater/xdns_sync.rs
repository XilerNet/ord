use std::sync::Mutex;
use crate::inscription::TransactionInscription;
use crate::inscription_id::InscriptionId;
use bitcoin::Address;
use db::XDNSRepository;
use tokio::runtime::Runtime;
use xdns_data::models::{Data, Domain, DomainDrop, SubDomain, Validity, ValidityTransfer};
use xdns_data::parser::{ActionParser, DomainAction};
use xdns_data::traits::Parser;

static mut DATABASE: Option<db::Repository> = None;

async fn get_db() -> &'static db::Repository {
  unsafe {
    if DATABASE.is_none() {
      DATABASE = Some(db::Repository::new().await);
      DATABASE.as_ref().unwrap().migrate().await;
    }

    DATABASE.as_ref().unwrap()
  }
}

async fn get_domain_by_inscription_id(db: &db::Repository, inscription: &str) -> Option<String> {
  let db_domain = db.get_domain_by_inscription(inscription).await;

  if db_domain.is_err() {
    let db_subdomain = db.get_subdomain_by_inscription(inscription).await;

    if db_subdomain.is_err() {
      return None;
    }

    let db_subdomain = db_subdomain.unwrap();
    return Some(db_subdomain.1.domain);
  }

  let db_domain = db_domain.unwrap();
  return Some(db_domain.1.name);
}

fn is_no_signature_required(actions: &Vec<DomainAction>) -> bool {
  (actions.len() == 1 && matches!(actions[0], DomainAction::Domain(_)))
    || (actions.len() == 2
    && ((matches!(actions[0], DomainAction::Domain(_))
    && matches!(actions[1], DomainAction::Validity(_)))
    || (matches!(actions[0], DomainAction::Validity(_))
    && matches!(actions[1], DomainAction::Domain(_)))))
}

async fn is_action_allowed(db: &db::Repository, action: &DomainAction, buffer: &Mutex<Option<String>>) -> bool {
  let action_domain = match action {
    DomainAction::Domain(Domain { name, .. }) => Some(name.to_string()),
    DomainAction::Subdomain(SubDomain { domain, .. }) => Some(domain.to_string()),
    DomainAction::Drop(DomainDrop { ref inscription }) => get_domain_by_inscription_id(&db, inscription).await,
    DomainAction::Validity(Validity { domain, .. }) => Some(domain.to_string()),
    DomainAction::ValidityTransfer(ValidityTransfer { domain, .. }) => Some(domain.to_string()),
    DomainAction::Data(Data { domain, .. }) => Some(domain.to_string()),
  };

  let mut tmp_domain = buffer.lock().unwrap();

  if let Some(action_domain) = action_domain {
    if let Some(expected_domain) = tmp_domain.clone() {
      if action_domain != expected_domain {
        return false;
      }
    } else {
      *tmp_domain = Some(action_domain);
    }

    return true;
  }

  false
}

pub(crate) async fn new_inscription(
  id: &InscriptionId,
  tx: Option<&TransactionInscription>,
  address: Option<Address>,
) {
  let db = get_db().await;
  if tx.is_none() || address.is_none() {
    return;
  }

  let tx = tx.unwrap();
  let address = address
    .map(|a| a.to_string())
    .unwrap_or_else(|| "unknown".into());
  let content_type = tx.inscription.content_type().unwrap_or("unknown");

  println!(
    "New inscription with id {} from {} with content type {}",
    id,
    address,
    content_type
  );

  if !content_type.starts_with("text/plain") {
    return;
  }

  let content = tx.inscription.body().unwrap_or(&[]);
  let content = String::from_utf8_lossy(content).to_string();
  println!("Content: {}", content);
  let parsed = ActionParser::parse(&content);

  if parsed.is_err() {
    return;
  }

  let parsed = parsed.unwrap();

  if parsed.actions.len() == 0 {
    return;
  }

  let signature_required = !is_no_signature_required(&parsed.actions);

  if parsed.signature.is_none() && signature_required {
    return;
  }

  let tmp_domain: Mutex<Option<String>> = Mutex::new(None);
  let mut actions = Vec::new();

  for action in parsed.actions {
    if !is_action_allowed(&db, &action, &tmp_domain).await {
      return;
    }

    actions.push(action);
  }

  let tmp_domain = tmp_domain.lock().unwrap();
  let domain = tmp_domain.clone();
  drop(tmp_domain);

  if signature_required {
    if parsed.signature.is_none() || domain.is_none() {
      return;
    }

    let domain = domain.unwrap();

    let signature = parsed.signature.unwrap();
    let validity = db.get_validity(&domain).await;

    if validity.is_err() {
      return;
    }

    let validity = validity.unwrap();

    if validity.0 != address.to_string() {
      return;
    }

    if !signature.is_valid(validity.1.credentials) {
      return;
    }
  }

  for action in actions {
    match action {
      DomainAction::Domain(domain) => {
        let name = domain.name.clone();
        let success = db.add_domain(&address, &id.to_string(), domain).await;

        if success {
          println!("Added domain {} for address {}", name, address);
        } else {
          println!("Failed to add domain {} for address {}", name, address);
        }
      }
      DomainAction::Subdomain(subdomain) => {
        let subdomain_representation = format!("{}{}", subdomain.subdomain.clone(), subdomain.domain.clone());
        let success = db.add_subdomain(&address, &id.to_string(), subdomain).await;

        if success {
          println!("Added subdomain {} for address {}", subdomain_representation, address);
        } else {
          println!("Failed to add subdomain {} for address {}", subdomain_representation, address);
        }
      }
      DomainAction::Drop(DomainDrop { inscription}) => {
        let success = db.remove_domain_by_inscription(&inscription).await;
        if success {
          println!("Removed domain with inscription id {} for address {}", inscription, address);
          return;
        }

        let success = db.remove_subdomain(&inscription).await;
        if success {
          println!("Removed subdomain with inscription id {} for address {}", inscription, address);
          return;
        }

        println!("Failed to remove domain or subdomain with inscription id {} for address {}", inscription, address);
      }
      DomainAction::Validity(validity) => {
        let domain = validity.domain.clone();
        let success = db.add_validity(&address, &id.to_string(), validity).await;

        if success {
          println!("Added validity {} for address {}", domain, address);
        } else {
          println!("Failed to add validity {} for address {}", domain, address);
        }
      }
      DomainAction::ValidityTransfer(transfer) => {
        let domain = transfer.domain.clone();
        let success = db.update_validity_by_inscription(&address, &id.to_string(), transfer).await;

        if success {
          println!("Added validity transfer {} for address {}", domain, address);
        } else {
          println!("Failed to add validity transfer {} for address {}", domain, address);
        }
      }
      DomainAction::Data(data) => {
        let domain = data.domain.clone();
        let success = db.add_data(&address, &id.to_string(), data).await;

        if success {
          println!("Added data {} for address {}", domain, address);
        } else {
          println!("Failed to add data {} for address {}", domain, address);
        }
      }
    }
  }
}

pub(crate) fn new_inscription_blocking(
  id: &InscriptionId,
  tx: Option<&TransactionInscription>,
  address: Option<Address>,
) {
  let rt = Runtime::new().unwrap();
  rt.block_on(new_inscription(id, tx, address));
}
