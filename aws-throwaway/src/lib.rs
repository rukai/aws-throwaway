mod cpu_arch;
mod ec2_instance;
mod ec2_instance_definition;
mod iam;
mod ssh;
mod tags;

use aws_config::meta::region::RegionProviderChain;
use aws_config::SdkConfig;
use aws_sdk_ec2::config::Region;
use aws_sdk_ec2::types::{
    BlockDeviceMapping, EbsBlockDevice, Filter, InstanceNetworkInterfaceSpecification, KeyType,
    Placement, PlacementStrategy, ResourceType, VolumeType,
};
use base64::Engine;
use ssh_key::rand_core::OsRng;
use ssh_key::PrivateKey;
use std::time::{Duration, Instant};
use tags::Tags;
use uuid::Uuid;

pub use aws_sdk_ec2::types::InstanceType;
pub use ec2_instance::{Ec2Instance, NetworkInterface};
pub use ec2_instance_definition::{Ec2InstanceDefinition, InstanceOs};
pub use tags::CleanupResources;

const AZ: &str = "us-east-1c";

async fn config() -> SdkConfig {
    let region_provider = RegionProviderChain::first_try(Region::new("us-east-1"));
    aws_config::from_env().region(region_provider).load().await
}

/// Construct this type to create and cleanup aws resources.
pub struct Aws {
    client: aws_sdk_ec2::Client,
    keyname: String,
    client_private_key: String,
    host_public_key: String,
    host_public_key_bytes: Vec<u8>,
    host_private_key: String,
    security_group_id: String,
    placement_group_name: String,
    subnet_id: String,
    tags: Tags,
}

impl Aws {
    /// Construct a new [`Aws`]
    ///
    /// Before returning the [`Aws`], all preexisting resources conforming to the specified [`CleanupResources`] approach are destroyed.
    /// The specified [`CleanupResources`] is then also used by the [`Aws::cleanup_resources`] method.
    ///
    /// All resources will be created in us-east-1c.
    /// This is hardcoded so that aws-throawaway only has to look into one region when cleaning up.
    /// All instances are created in a single spread placement group in a single AZ to ensure consistent latency between instances.
    ///
    /// If vpc_id is:
    /// * Some(_) => all resources will go into that vpc
    /// * None => all resources will go into the default vpc
    /// If subnet_id is:
    /// * Some(_) => all instances will go into that subnet
    /// * None => all instances will go into the default subnet for the specified or default vpc
    pub async fn new(
        cleanup: CleanupResources,
        vpc_id: Option<String>,
        subnet_id: Option<String>,
    ) -> Self {
        let config = config().await;
        let user_name = iam::user_name(&config).await;
        let keyname = format!("aws-throwaway-{user_name}-{}", Uuid::new_v4());
        let security_group_name = format!("aws-throwaway-{user_name}-{}", Uuid::new_v4());
        let placement_group_name = format!("aws-throwaway-{user_name}-{}", Uuid::new_v4());
        let client = aws_sdk_ec2::Client::new(&config);

        let tags = Tags {
            user_name: user_name.clone(),
            cleanup,
        };

        // Cleanup any resources that were previously failed to cleanup
        Self::cleanup_resources_inner(&client, &tags).await;

        let (client_private_key, security_group_id, _, default_subnet_id) = tokio::join!(
            Aws::create_key_pair(&client, &tags, &keyname),
            Aws::create_security_group(&client, &tags, &security_group_name, &vpc_id),
            Aws::create_placement_group(&client, &tags, &placement_group_name),
            Aws::get_default_subnet_id(&client, subnet_id)
        );

        let key = PrivateKey::random(&mut OsRng {}, ssh_key::Algorithm::Ed25519).unwrap();
        let host_public_key_bytes = key.public_key().to_bytes().unwrap();
        let host_public_key = key.public_key().to_openssh().unwrap();
        let host_private_key = key.to_openssh(ssh_key::LineEnding::LF).unwrap().to_string();

        Aws {
            client,
            keyname,
            client_private_key,
            host_public_key_bytes,
            host_public_key,
            host_private_key,
            security_group_id,
            placement_group_name,
            subnet_id: default_subnet_id,
            tags,
        }
    }

    async fn create_key_pair(client: &aws_sdk_ec2::Client, tags: &Tags, name: &str) -> String {
        let keypair = client
            .create_key_pair()
            .key_name(name)
            .key_type(KeyType::Ed25519)
            .tag_specifications(tags.create_tags(ResourceType::KeyPair, "aws-throwaway"))
            .send()
            .await
            .map_err(|e| e.into_service_error())
            .unwrap();
        let client_private_key = keypair.key_material().unwrap().to_string();
        tracing::info!("client_private_key:\n{}", client_private_key);
        client_private_key
    }

    async fn create_security_group(
        client: &aws_sdk_ec2::Client,
        tags: &Tags,
        name: &str,
        vpc_id: &Option<String>,
    ) -> String {
        let security_group_id = client
            .create_security_group()
            .group_name(name)
            .set_vpc_id(vpc_id.clone())
            .description("aws-throwaway security group")
            .tag_specifications(tags.create_tags(ResourceType::SecurityGroup, "aws-throwaway"))
            .send()
            .await
            .map_err(|e| e.into_service_error())
            .unwrap()
            .group_id
            .unwrap();
        tracing::info!("created security group");

        tokio::join!(
            Aws::create_ingress_rule_internal(client, tags, name),
            Aws::create_ingress_rule_ssh(client, tags, name),
        );

        security_group_id
    }

    async fn create_ingress_rule_internal(
        client: &aws_sdk_ec2::Client,
        tags: &Tags,
        group_name: &str,
    ) {
        assert!(client
            .authorize_security_group_ingress()
            .group_name(group_name)
            .source_security_group_name(group_name)
            .tag_specifications(
                tags.create_tags(ResourceType::SecurityGroupRule, "within aws-throwaway SG")
            )
            .send()
            .await
            .map_err(|e| e.into_service_error())
            .unwrap()
            .r#return()
            .unwrap());
        tracing::info!("created security group rule - internal");
    }

    async fn create_ingress_rule_ssh(client: &aws_sdk_ec2::Client, tags: &Tags, group_name: &str) {
        assert!(client
            .authorize_security_group_ingress()
            .group_name(group_name)
            .ip_protocol("tcp")
            .from_port(22)
            .to_port(22)
            .cidr_ip("0.0.0.0/0")
            .tag_specifications(tags.create_tags(ResourceType::SecurityGroupRule, "ssh"))
            .send()
            .await
            .map_err(|e| e.into_service_error())
            .unwrap()
            .r#return()
            .unwrap());
        tracing::info!("created security group rule - ssh");
    }

    async fn create_placement_group(client: &aws_sdk_ec2::Client, tags: &Tags, name: &str) {
        client
            .create_placement_group()
            .group_name(name)
            // refer to: https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/placement-groups.html
            // For our current usage spread makes the most sense.
            .strategy(PlacementStrategy::Spread)
            .tag_specifications(tags.create_tags(ResourceType::PlacementGroup, "aws-throwaway"))
            .send()
            .await
            .map_err(|e| e.into_service_error())
            .unwrap();
        tracing::info!("created placement group");
    }

    async fn get_default_subnet_id(
        client: &aws_sdk_ec2::Client,
        subnet_id: Option<String>,
    ) -> String {
        match subnet_id {
            Some(subnet_id) => subnet_id,
            None => client
                .describe_subnets()
                .filters(
                    Filter::builder()
                        .name("default-for-az")
                        .values("true")
                        .build(),
                )
                .filters(
                    Filter::builder()
                        .name("availability-zone")
                        .values(AZ)
                        .build(),
                )
                .send()
                .await
                .map_err(|e| e.into_service_error())
                .unwrap()
                .subnets
                .unwrap()
                .pop()
                .unwrap()
                .subnet_id
                .unwrap(),
        }
    }

    /// Call before dropping [`Aws`]
    /// Uses the `CleanupResources` method specified in the constructor.
    pub async fn cleanup_resources(&self) {
        Self::cleanup_resources_inner(&self.client, &self.tags).await
    }

    /// Call to cleanup without constructing an [`Aws`]
    pub async fn cleanup_resources_static(cleanup: CleanupResources) {
        let config = config().await;
        let user_name = iam::user_name(&config).await;
        let client = aws_sdk_ec2::Client::new(&config);
        let tags = Tags { user_name, cleanup };
        Aws::cleanup_resources_inner(&client, &tags).await;
    }

    async fn get_all_throwaway_tags(
        client: &aws_sdk_ec2::Client,
        tags: &Tags,
        resource_type: &str,
    ) -> Vec<String> {
        let (user_tags, app_tags) = tokio::join!(
            tags.fetch_user_tags(client, resource_type),
            tags.fetch_app_tags(client, resource_type),
        );

        let mut ids_of_user = vec![];
        for tag in user_tags.tags().unwrap() {
            if let Some(id) = tag.resource_id() {
                ids_of_user.push(id.to_owned());
            }
        }

        if let Some(app_tags) = app_tags {
            let mut ids_of_user_and_app = vec![];
            for app_tag in app_tags.tags().unwrap() {
                if let Some(id) = app_tag.resource_id() {
                    let id = id.to_owned();
                    if ids_of_user.contains(&id) {
                        ids_of_user_and_app.push(id);
                    }
                }
            }
            ids_of_user_and_app
        } else {
            ids_of_user
        }
    }

    async fn cleanup_resources_inner(client: &aws_sdk_ec2::Client, tags: &Tags) {
        // release elastic ips
        for id in Self::get_all_throwaway_tags(client, tags, "elastic-ip").await {
            client
                .release_address()
                .allocation_id(&id)
                .send()
                .await
                .map_err(|e| {
                    anyhow::anyhow!(e.into_service_error())
                        .context(format!("Failed to release elastic ip {id:?}"))
                })
                .unwrap();
            tracing::info!("elastic ip {id:?} was succesfully deleted");
        }

        tracing::info!("Terminating instances");
        let instance_ids = Self::get_all_throwaway_tags(client, tags, "instance").await;
        if !instance_ids.is_empty() {
            for result in client
                .terminate_instances()
                .set_instance_ids(Some(instance_ids))
                .send()
                .await
                .map_err(|e| e.into_service_error())
                .unwrap()
                .terminating_instances()
                .unwrap()
            {
                tracing::info!(
                    "Instance {:?} {:?} -> {:?}",
                    result.instance_id.as_ref().unwrap(),
                    result.previous_state().unwrap().name().unwrap(),
                    result.current_state().unwrap().name().unwrap()
                );
            }
        }

        tokio::join!(
            Aws::delete_security_groups(client, tags),
            Aws::delete_placement_groups(client, tags),
        );
    }

    async fn delete_security_groups(client: &aws_sdk_ec2::Client, tags: &Tags) {
        for id in Self::get_all_throwaway_tags(client, tags, "security-group").await {
            if let Err(err) = client.delete_security_group().group_id(&id).send().await {
                tracing::info!(
                    "security group {id:?} could not be deleted, this will get cleaned up eventually on a future aws-throwaway cleanup: {:?}",
                    err.into_service_error().meta().message()
                )
            } else {
                tracing::info!("security group {id:?} was succesfully deleted")
            }
        }
    }

    async fn delete_placement_groups(client: &aws_sdk_ec2::Client, tags: &Tags) {
        let placement_group_ids =
            Self::get_all_throwaway_tags(client, tags, "placement-group").await;
        if !placement_group_ids.is_empty() {
            // placement groups can not be deleted by id so we must look up their names
            let placement_groups = client
                .describe_placement_groups()
                .set_group_ids(Some(placement_group_ids))
                .send()
                .await
                .map_err(|e| e.into_service_error())
                .unwrap();
            for placement_group in placement_groups.placement_groups().unwrap() {
                let name = placement_group.group_name().unwrap();
                if let Err(err) = client
                    .delete_placement_group()
                    .group_name(name)
                    .send()
                    .await
                {
                    tracing::info!(
                    "placement group {name:?} could not be deleted, this will get cleaned up eventually on a future aws-throwaway cleanup: {:?}",
                    err.into_service_error().meta().message()
                )
                } else {
                    tracing::info!("placement group {name:?} was succesfully deleted")
                }
            }
        }
    }

    /// Creates a new EC2 instance as defined by [`Ec2InstanceDefinition`]
    pub async fn create_ec2_instance(&self, definition: Ec2InstanceDefinition) -> Ec2Instance {
        // elastic IP's are a limited resource so only create it if we truly need it.
        let elastic_ip = if definition.network_interface_count > 1 {
            Some(
                self.client
                    .allocate_address()
                    .tag_specifications(
                        self.tags
                            .create_tags(ResourceType::ElasticIp, "aws-throwaway"),
                    )
                    .send()
                    .await
                    .map_err(|e| e.into_service_error())
                    .unwrap(),
            )
        } else {
            None
        };

        // if we specify a list of network interfaces we cannot specify an instance level security group
        let security_group_ids = if elastic_ip.is_some() {
            None
        } else {
            Some(vec![self.security_group_id.clone()])
        };

        let ubuntu_version = match definition.os {
            InstanceOs::Ubuntu20_04 => "20.04",
            InstanceOs::Ubuntu22_04 => "22.04",
        };
        let image_id = format!(
            "resolve:ssm:/aws/service/canonical/ubuntu/server/{}/stable/current/{}/hvm/ebs-gp2/ami-id",
            ubuntu_version,
            cpu_arch::get_arch_of_instance_type(definition.instance_type.clone()).get_ubuntu_arch_identifier()
        );
        let result = self
            .client
            .run_instances()
            .instance_type(definition.instance_type)
            .set_placement(Some(
                Placement::builder()
                    .group_name(&self.placement_group_name)
                    .availability_zone(AZ)
                    .build(),
            ))
            .set_subnet_id(if elastic_ip.is_some() {
                None
            } else {
                Some(self.subnet_id.to_owned())
            })
            .min_count(1)
            .max_count(1)
            .block_device_mappings(
                BlockDeviceMapping::builder()
                    .device_name("/dev/sda1")
                    .ebs(
                        EbsBlockDevice::builder()
                            .delete_on_termination(true)
                            .volume_size(definition.volume_size_gb as i32)
                            .volume_type(VolumeType::Gp2)
                            .build(),
                    )
                    .build(),
            )
            .set_security_group_ids(security_group_ids)
            .set_network_interfaces(if elastic_ip.is_some() {
                Some(
                    (0..definition.network_interface_count)
                        .map(|i| {
                            InstanceNetworkInterfaceSpecification::builder()
                                .delete_on_termination(true)
                                .device_index(i as i32)
                                .groups(&self.security_group_id)
                                .associate_public_ip_address(false)
                                .subnet_id(&self.subnet_id)
                                .description(i.to_string())
                                .build()
                        })
                        .collect(),
                )
            } else {
                None
            })
            .key_name(&self.keyname)
            .user_data(base64::engine::general_purpose::STANDARD.encode(format!(
                r#"#!/bin/bash
sudo systemctl stop ssh
echo "{}" > /etc/ssh/ssh_host_ed25519_key.pub
echo "{}" > /etc/ssh/ssh_host_ed25519_key

echo "ClientAliveInterval 30" >> /etc/ssh/sshd_config
sudo systemctl start ssh
            "#,
                self.host_public_key, self.host_private_key
            )))
            .tag_specifications(
                self.tags
                    .create_tags(ResourceType::Instance, "aws-throwaway"),
            )
            .image_id(image_id)
            .send()
            .await
            .map_err(|e| e.into_service_error())
            .unwrap();

        let instance = result.instances().unwrap().first().unwrap();
        let primary_network_interface_id = instance
            .network_interfaces
            .as_ref()
            .unwrap()
            .iter()
            .find(|x| x.attachment.as_ref().unwrap().device_index.unwrap() == 0)
            .unwrap()
            .network_interface_id
            .as_ref()
            .unwrap();

        let network_interfaces = instance
            .network_interfaces
            .as_ref()
            .unwrap()
            .iter()
            .map(|x| NetworkInterface {
                device_index: x.attachment.as_ref().unwrap().device_index.unwrap(),
                private_ipv4: x.private_ip_address.as_ref().unwrap().parse().unwrap(),
            })
            .collect();

        if let Some(elastic_ip) = &elastic_ip {
            let start = Instant::now();
            loop {
                match self
                    .client
                    .associate_address()
                    .allocation_id(elastic_ip.allocation_id.as_ref().unwrap())
                    .network_interface_id(primary_network_interface_id)
                    .send()
                    .await
                {
                    Ok(_) => {
                        break;
                    }
                    Err(err) => {
                        // It is expected to receive the following error if we attempt too early:
                        // `The pending-instance-running instance to which 'eni-***' is attached is not in a valid state for this operation`
                        if start.elapsed() > Duration::from_secs(120) {
                            panic!(
                                "Received error while assosciating address after 120s retrying: {}",
                                err.into_service_error()
                            );
                        } else {
                            tokio::time::sleep(Duration::from_secs(2)).await;
                        }
                    }
                }
            }
        }

        let mut public_ip = elastic_ip.map(|x| x.public_ip.unwrap().parse().unwrap());
        let mut private_ip = None;

        while public_ip.is_none() || private_ip.is_none() {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            for reservation in self
                .client
                .describe_instances()
                .instance_ids(instance.instance_id().unwrap())
                .send()
                .await
                .map_err(|e| e.into_service_error())
                .unwrap()
                .reservations()
                .unwrap()
            {
                for instance in reservation.instances().unwrap() {
                    if public_ip.is_none() {
                        public_ip = instance.public_ip_address().map(|x| x.parse().unwrap());
                    }
                    private_ip = instance.private_ip_address().map(|x| x.parse().unwrap());
                }
            }
        }
        let public_ip = public_ip.unwrap();
        let private_ip = private_ip.unwrap();
        tracing::info!("created EC2 instance at: {public_ip}");

        Ec2Instance::new(
            public_ip,
            private_ip,
            self.host_public_key_bytes.clone(),
            self.host_public_key.clone(),
            &self.client_private_key,
            network_interfaces,
        )
        .await
    }
}
