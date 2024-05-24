use frp_operator::controllers::client;
use kube::CustomResourceExt;

fn main() {
    print!("{}", serde_yaml::to_string(&client::Client::crd()).unwrap())
}
